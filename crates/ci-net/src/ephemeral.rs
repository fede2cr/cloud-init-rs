//! `cloudinit.net.ephemeral`: a temporary IPv4 configuration, held only long
//! enough to fetch metadata.
//!
//! This is the piece that turns [`crate::dhcp`]'s lease into a machine that can
//! reach a metadata service, and — the part that matters — takes it away again
//! afterwards, so the boot's real network configuration is applied to an
//! untouched interface.
//!
//! Upstream is a pair of context managers. Here [`Ephemeral`] owns the same
//! cleanup list and undoes it in `Drop`, which is the closer analogue of
//! `__exit__` than an explicit teardown call would be: a `?` anywhere in the
//! caller still restores the interface.
//!
//! Only the IPv4 half is ported. `EphemeralIPv6Network` and the combined
//! `EphemeralIPNetwork` are not (deviation 113); nothing that reaches this
//! code needs them, because both Azure's wireserver and IMDS are IPv4.

use std::net::Ipv4Addr;

use ci_config::{Object, Value};

use crate::netinfo::{self, Device};
use crate::netops;

const SOURCE: &str = "ephemeral.py";

/// Why an ephemeral network could not be set up.
#[derive(Debug)]
pub enum Error {
    /// `NoDHCPLeaseError`.
    NoLease(String),
    /// `NoDHCPLeaseInterfaceError`: there was no NIC to try.
    NoInterface,
    /// `NoDHCPLeaseMissingDhclientError`. Retrying cannot help.
    MissingClient,
    /// The lease was obtained but could not be applied.
    Setup(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLease(reason) | Self::Setup(reason) => f.write_str(reason),
            Self::NoInterface => f.write_str("Unable to find fallback nic"),
            Self::MissingClient => f.write_str("dhcpcd executable not found"),
        }
    }
}

/// One undo step, queued while the interface is being brought up.
#[derive(Debug)]
enum Undo {
    LinkDown(String),
    DelAddr {
        interface: String,
        cidr: String,
    },
    DelRoute {
        interface: String,
        address: String,
        gateway: Option<String>,
        source: Option<String>,
    },
}

/// `EphemeralIPv4Network` plus the `EphemeralDHCPv4` that produced it.
///
/// Dropping this undoes everything it did, in reverse order of the list
/// upstream builds.
#[derive(Debug)]
pub struct Ephemeral {
    /// The lease `dhcpcd` handed back, with dhclient-style key names.
    pub lease: Object,
    pub interface: String,
    pub ip: String,
    pub prefix: u8,
    pub router: Option<String>,
    pub static_routes: Vec<(String, String)>,
    undo: Vec<Undo>,
}

impl Ephemeral {
    /// `self.cidr`.
    #[must_use]
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.ip, self.prefix)
    }

    /// `lease["unknown-245"]`, the Azure wireserver endpoint.
    #[must_use]
    pub fn wireserver_endpoint(&self) -> Option<&str> {
        self.lease.get("unknown-245").and_then(Value::as_str)
    }
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        // Upstream's `cleanup_cmds` is built with `append` for the device and
        // `insert(0, ...)` for the routes, so routes come down first. Undoing
        // in reverse insertion order gives the same sequence without the two
        // different insert positions.
        let mut log = ci_log::Logger::silent();
        for step in self.undo.drain(..).rev() {
            let result = match &step {
                Undo::LinkDown(interface) => netops::link_down(interface, Some("inet")),
                Undo::DelAddr { interface, cidr } => netops::del_addr(interface, cidr),
                Undo::DelRoute {
                    interface,
                    address,
                    gateway,
                    source,
                } => netops::del_route(
                    interface,
                    address,
                    gateway.as_deref(),
                    source.as_deref(),
                ),
            };
            if let Err(error) = result {
                log.warning(SOURCE, &format!("Ephemeral teardown: {error}"));
            }
        }
    }
}

/// `EphemeralDHCPv4.obtain_lease`: discover a lease and apply it.
///
/// `interface` is upstream's `iface`; `None` means "pick one", which is
/// `distro.fallback_interface`.
///
/// # Errors
/// [`Error`] describing which of upstream's four `NoDHCPLease*` cases occurred.
pub fn obtain_lease(
    interface: Option<&str>,
    sys: &crate::sysfs::Sys,
    log: &mut ci_log::Logger,
) -> Result<Ephemeral, Error> {
    let interface = match interface {
        Some(interface) => interface.to_owned(),
        None => sys.fallback_nic().ok_or_else(|| {
            log.debug(SOURCE, "Skip dhcp_discovery: Unable to find fallback nic.");
            Error::NoInterface
        })?,
    };

    let before = netinfo::netdev_info();
    let lease =
        crate::dhcp::discover(&interface, log).map_err(|error| match error {
            crate::dhcp::Error::MissingClient => Error::MissingClient,
            other => Error::NoLease(other.to_string()),
        })?;
    apply(lease, &before, log)
}

/// The half of `obtain_lease` after the client has answered: turn the lease
/// into addresses and routes.
///
/// Split out so it can be tested without a DHCP server — the mapping from
/// lease keys to `ip` arguments is where the mistakes live, not in the
/// subprocess.
fn apply(
    lease: Object,
    before: &std::collections::BTreeMap<String, Device>,
    log: &mut ci_log::Logger,
) -> Result<Ephemeral, Error> {
    let plan = Plan::from_lease(&lease)?;
    log.debug(
        SOURCE,
        &format!(
            "Received dhcp lease on {} for {}/{}",
            plan.interface, plan.ip, plan.mask
        ),
    );

    let prefix = crate::ip::ipv4_mask_to_net_prefix(&plan.mask).ok_or_else(|| {
        Error::Setup(format!(
            "Cannot setup network, invalid prefix or netmask: {}",
            plan.mask
        ))
    })?;
    let cidr = format!("{}/{prefix}", plan.ip);

    let mut ephemeral = Ephemeral {
        interface: plan.interface.clone(),
        ip: plan.ip.clone(),
        prefix,
        router: plan.router.clone(),
        static_routes: plan.static_routes.clone(),
        lease,
        undo: Vec::new(),
    };

    bringup_device(&mut ephemeral, &plan, &cidr, before, log)?;

    // rfc3442: when classless static routes are present the router option
    // MUST be ignored.
    if plan.static_routes.is_empty() {
        if let Some(router) = &plan.router {
            bringup_router(&mut ephemeral, router, log);
        }
    } else {
        for (address, gateway) in &plan.static_routes {
            if let Err(error) = netops::append_route(&plan.interface, address, gateway)
            {
                return Err(Error::Setup(error.to_string()));
            }
            ephemeral.undo.push(Undo::DelRoute {
                interface: plan.interface.clone(),
                address: address.clone(),
                gateway: Some(gateway.clone()),
                source: None,
            });
        }
    }

    Ok(ephemeral)
}

/// `_bringup_device`: give the interface the address and the link state the
/// lease implies, and queue the undo for whichever of the two was not already
/// there.
///
/// `before` is the state from *before* the DHCP client ran; the current state
/// is read again here, because the client may itself have brought the link up
/// in between. That difference is the whole point of the function: what was
/// true before decides what gets torn down, what is true now decides what gets
/// configured.
fn bringup_device(
    ephemeral: &mut Ephemeral,
    plan: &Plan,
    cidr: &str,
    before: &std::collections::BTreeMap<String, Device>,
    log: &mut ci_log::Logger,
) -> Result<(), Error> {
    let prior = before.get(&plan.interface);
    let was_up = prior.is_some_and(|device| device.up);
    let was_addressed = prior
        .is_some_and(|device| device.ipv4.iter().any(|address| address.ip == plan.ip));

    let now = netinfo::netdev_info();
    let current = now.get(&plan.interface);
    let is_up = current.is_some_and(|device| device.up);
    let is_addressed = current
        .is_some_and(|device| device.ipv4.iter().any(|address| address.ip == plan.ip));

    if is_addressed {
        log.debug(
            SOURCE,
            &format!(
                "Skip adding ip address: {} already has address {}",
                plan.interface, plan.ip
            ),
        );
    } else if let Err(error) =
        netops::add_addr(&plan.interface, cidr, Some(&plan.broadcast))
    {
        // `File exists` / `Address already assigned` is not a failure: it is
        // the client having got there first.
        if !error.reason.contains("File exists")
            && !error.reason.contains("Address already assigned")
        {
            return Err(Error::Setup(error.to_string()));
        }
    }

    if is_up {
        log.debug(
            SOURCE,
            &format!(
                "Skip bringing up network link: interface {} is already up",
                plan.interface
            ),
        );
    } else if let Err(error) = netops::link_up(&plan.interface, Some("inet")) {
        return Err(Error::Setup(error.to_string()));
    }

    if was_up {
        log.debug(
            SOURCE,
            &format!(
                "Not queueing link down: link [{}] was up prior before \
                 receiving a dhcp lease",
                plan.interface
            ),
        );
    } else {
        ephemeral.undo.push(Undo::LinkDown(plan.interface.clone()));
    }

    if was_addressed {
        log.debug(
            SOURCE,
            &format!(
                "Not queueing address removal: address {} was assigned before \
                 receiving a dhcp lease",
                plan.ip
            ),
        );
    } else {
        ephemeral.undo.push(Undo::DelAddr {
            interface: plan.interface.clone(),
            cidr: cidr.to_owned(),
        });
    }

    Ok(())
}

/// `_bringup_router`.
///
/// A failure here is logged rather than fatal: upstream would raise, but the
/// lease and address are already in place and a machine with an address and no
/// default route can still reach a link-local metadata service. Tearing the
/// whole thing down for that would turn a usable boot into a failed one.
fn bringup_router(ephemeral: &mut Ephemeral, router: &str, log: &mut ci_log::Logger) {
    if let Ok(existing) = netops::default_route() {
        if existing.contains("default") {
            log.debug(
                SOURCE,
                &format!(
                    "Skip ephemeral route setup. {} already has default route: {}",
                    ephemeral.interface,
                    existing.trim()
                ),
            );
            return;
        }
    }

    let interface = ephemeral.interface.clone();
    let ip = ephemeral.ip.clone();
    // The router itself first, reachable on-link from our address, then the
    // default route through it.
    for (address, gateway, source) in [
        (router.to_owned(), None, Some(ip)),
        ("default".to_owned(), Some(router.to_owned()), None),
    ] {
        match netops::add_route(
            &interface,
            &address,
            gateway.as_deref(),
            source.as_deref(),
        ) {
            Ok(()) => ephemeral.undo.push(Undo::DelRoute {
                interface: interface.clone(),
                address,
                gateway,
                // `ip route del default` takes no `src`, matching upstream's
                // `partial(del_route, interface, "default")`.
                source,
            }),
            Err(error) => {
                log.warning(SOURCE, &format!("Failed to add ephemeral route: {error}"));
                return;
            }
        }
    }
}

/// The lease fields `EphemeralIPv4Network` is constructed from, after
/// upstream's `nmap` rename.
#[derive(Debug)]
struct Plan {
    interface: String,
    ip: String,
    mask: String,
    broadcast: String,
    router: Option<String>,
    static_routes: Vec<(String, String)>,
}

impl Plan {
    fn from_lease(lease: &Object) -> Result<Self, Error> {
        let text = |key: &str| {
            lease
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };

        let interface = text("interface")
            .ok_or_else(|| Error::Setup("lease has no interface".to_owned()))?;
        let ip = text("fixed-address")
            .ok_or_else(|| Error::Setup("lease has no address".to_owned()))?;
        let mask = text("subnet-mask")
            .ok_or_else(|| Error::Setup("lease has no subnet mask".to_owned()))?;

        // `get_first_option_value`: the same routes under four names,
        // depending on which client and which option produced them.
        let routes = [
            "rfc3442-classless-static-routes",
            "classless-static-routes",
            "static_routes",
            "unknown-121",
        ]
        .into_iter()
        .find_map(text)
        .unwrap_or_default();

        let broadcast = text("broadcast-address")
            .or_else(|| mask_and_ipv4_to_bcast_addr(&mask, &ip))
            .ok_or_else(|| {
                Error::Setup(format!("cannot derive a broadcast address for {ip}"))
            })?;

        let mut warnings = ci_log::Logger::silent();
        Ok(Self {
            interface,
            ip,
            mask,
            broadcast,
            router: text("routers"),
            static_routes: crate::dhcp::parse_static_routes(&routes, &mut warnings),
        })
    }
}

/// `net.mask_and_ipv4_to_bcast_addr`.
fn mask_and_ipv4_to_bcast_addr(mask: &str, ip: &str) -> Option<String> {
    let mask: Ipv4Addr = mask.parse().ok()?;
    let ip: Ipv4Addr = ip.parse().ok()?;
    let bcast = u32::from(ip) & u32::from(mask) | !u32::from(mask);
    Some(Ipv4Addr::from(bcast).to_string())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn lease(pairs: &[(&str, &str)]) -> Object {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::from(*v)))
            .collect()
    }

    fn base() -> Object {
        lease(&[
            ("interface", "eth0"),
            ("fixed-address", "10.0.0.4"),
            ("subnet-mask", "255.255.255.0"),
            ("routers", "10.0.0.1"),
        ])
    }

    #[test]
    fn a_lease_without_a_broadcast_address_gets_one_derived() {
        let plan = Plan::from_lease(&base()).unwrap();
        assert_eq!(plan.broadcast, "10.0.0.255");
        assert_eq!(plan.router.as_deref(), Some("10.0.0.1"));
        assert!(plan.static_routes.is_empty());
    }

    #[test]
    fn a_broadcast_address_in_the_lease_is_preferred_over_a_derived_one() {
        let mut lease = base();
        lease.insert("broadcast-address".to_owned(), Value::from("10.0.0.63"));
        assert_eq!(Plan::from_lease(&lease).unwrap().broadcast, "10.0.0.63");
    }

    #[test]
    fn the_routes_are_read_from_whichever_of_the_four_names_is_present() {
        let mut lease = base();
        lease.insert(
            "unknown-121".to_owned(),
            Value::from("169.254.169.254/32 10.0.0.1 0.0.0.0/0 10.0.0.1"),
        );
        let plan = Plan::from_lease(&lease).unwrap();
        assert_eq!(
            plan.static_routes,
            vec![
                ("169.254.169.254/32".to_owned(), "10.0.0.1".to_owned()),
                ("0.0.0.0/0".to_owned(), "10.0.0.1".to_owned()),
            ]
        );
    }

    #[test]
    fn a_lease_missing_an_address_or_a_mask_is_refused() {
        for missing in ["interface", "fixed-address", "subnet-mask"] {
            let mut lease = base();
            lease.shift_remove(missing);
            assert!(
                matches!(Plan::from_lease(&lease), Err(Error::Setup(_))),
                "{missing} should be required"
            );
        }
    }

    #[test]
    fn a_broadcast_address_is_the_host_bits_set() {
        assert_eq!(
            mask_and_ipv4_to_bcast_addr("255.255.255.0", "10.0.0.4").as_deref(),
            Some("10.0.0.255")
        );
        assert_eq!(
            mask_and_ipv4_to_bcast_addr("255.255.255.255", "10.0.0.4").as_deref(),
            Some("10.0.0.4")
        );
        assert_eq!(mask_and_ipv4_to_bcast_addr("nonsense", "10.0.0.4"), None);
    }

    #[test]
    fn the_wireserver_endpoint_comes_off_the_lease() {
        let mut lease = base();
        lease.insert("unknown-245".to_owned(), Value::from("168.63.129.16"));
        let ephemeral = Ephemeral {
            interface: "eth0".to_owned(),
            ip: "10.0.0.4".to_owned(),
            prefix: 24,
            router: None,
            static_routes: Vec::new(),
            lease,
            undo: Vec::new(),
        };
        assert_eq!(ephemeral.cidr(), "10.0.0.4/24");
        assert_eq!(ephemeral.wireserver_endpoint(), Some("168.63.129.16"));
    }
}
