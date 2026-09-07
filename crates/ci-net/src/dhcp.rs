//! `cloudinit.net.dhcp`: obtaining a lease before there is any network.
//!
//! Only the `dhcpcd` client is here. Upstream carries three (`dhclient`,
//! `dhcpcd`, `udhcpc`) and picks between them per distro; `dhcpcd` is the one
//! current Ubuntu and Debian images ship and the only one this port needs so
//! far, so the others are left for whoever needs them (deviation 112).
//!
//! The split in this module is deliberate: everything that turns bytes into a
//! lease is a free function taking those bytes, and only [`discover`] and
//! [`newest_lease`] run a program. That is what makes the interesting half —
//! option 245, which is how a machine on Azure learns its wireserver — testable
//! without a DHCP server.

use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::time::Duration;

use ci_config::{Object, Value};
use ci_sys::subp;

const SOURCE: &str = "dhcp.py";

/// `Dhcpcd.client_name`.
pub const CLIENT_NAME: &str = "dhcpcd";

/// `Dhcpcd.timeout`.
pub const TIMEOUT: Duration = Duration::from_secs(300);

/// Where `dhcpcd` leaves the raw lease packet, which is the only place the
/// options it does not understand survive.
#[must_use]
pub fn lease_packet_path(interface: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/var/lib/dhcpcd/{interface}.lease"))
}

/// The three ways a lease can fail to arrive, which upstream spells as three
/// exception classes.
#[derive(Debug)]
pub enum Error {
    /// `NoDHCPLeaseMissingDhclientError`.
    MissingClient,
    /// `NoDHCPLeaseError`.
    NoLease(String),
    /// `InvalidDHCPLeaseFileError`.
    InvalidLease(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingClient => write!(f, "dhcpcd executable not found"),
            Self::NoLease(reason) | Self::InvalidLease(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for Error {}

/// `repr()` of a string, which is how upstream interpolates the lease dump into
/// both its debug line and the error it raises.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `Dhcpcd.parse_unknown_options_from_packet`.
///
/// A DHCP packet is bootp plus options: the vendor area starts at byte 236 and
/// the magic cookie takes the next four, so the walk starts at 240. Each option
/// is a code byte, a length byte, and that many bytes of value.
#[must_use]
pub fn unknown_option(data: &[u8], code: u8) -> Option<&[u8]> {
    let mut index = 240usize;
    while data.len() >= index + 2 {
        let (Some(&found), Some(&length)) = (data.get(index), data.get(index + 1))
        else {
            return None;
        };
        let start = index + 2;
        // Python slices clamp; a truncated trailing option yields what is left.
        let end = start.saturating_add(length as usize).min(data.len());
        let option = data.get(start..end)?;
        if found == code {
            return Some(option);
        }
        index = start + length as usize;
    }
    None
}

/// `Dhcpcd.parse_static_routes`: `dest1/mask gw1 ... destn/mask gwn`.
#[must_use]
pub fn parse_static_routes(
    routes: &str,
    log: &mut ci_log::Logger,
) -> Vec<(String, String)> {
    let fields: Vec<&str> = routes.split_whitespace().collect();
    if fields.is_empty() {
        log.warning(
            SOURCE,
            &format!("Malformed classless static routes: [{routes}]"),
        );
        return Vec::new();
    }
    // `zip` stops at the shorter side, so a trailing destination is dropped.
    fields
        .chunks_exact(2)
        .filter_map(|pair| match pair {
            [dest, gateway] => Some(((*dest).to_owned(), (*gateway).to_owned())),
            _ => None,
        })
        .collect()
}

/// `Dhcpcd.parse_dhcpcd_lease`.
///
/// `packet` is the contents of [`lease_packet_path`]; upstream reads it inline,
/// but taking it as an argument is what lets a lease be parsed off a fixture.
pub fn parse_lease(
    dump: &str,
    interface: &str,
    packet: Option<&[u8]>,
    log: &mut ci_log::Logger,
) -> Result<Object, Error> {
    log.debug(
        SOURCE,
        &format!(
            "Parsing dhcpcd lease for interface {interface}: {}",
            py_repr(dump)
        ),
    );

    let mut lease = Object::new();
    for line in dump.trim().replace('\'', "").split('\n') {
        if let Some((key, value)) = line.split_once('=') {
            lease.insert(key.replace('_', "-"), Value::String(value.to_owned()));
        }
    }
    if lease.is_empty() {
        let message = format!(
            "No valid DHCP lease configuration found in dhcpcd lease: {}",
            py_repr(dump)
        );
        log.error(SOURCE, &message);
        return Err(Error::InvalidLease(message));
    }
    lease.insert("interface".to_owned(), Value::String(interface.to_owned()));

    // `isc-dhclient`'s names are what the rest of the codebase reads, and
    // `static_routes` keeps its underscore because `ephemeral.py` spells it
    // that way. Both moves push the key to the end, as `dict.pop` does.
    for (from, to) in [
        ("ip-address", "fixed-address"),
        ("classless-static-routes", "static_routes"),
    ] {
        // `shift_remove`, not `remove`: the latter is a swap-remove, which
        // would drag the last key into the hole and reorder the lease.
        if let Some(value) = lease.shift_remove(from) {
            lease.insert(to.to_owned(), value);
        }
    }

    if let Some(option) = packet.and_then(|data| unknown_option(data, 245)) {
        if let Ok(octets) = <[u8; 4]>::try_from(option) {
            lease.insert(
                "unknown-245".to_owned(),
                Value::String(Ipv4Addr::from(octets).to_string()),
            );
        } else if !option.is_empty() {
            // `socket.inet_ntoa` raises here; refusing the option leaves the
            // caller on its default endpoint rather than failing the boot.
            log.warning(
                SOURCE,
                &format!("Ignoring option 245 of {} bytes", option.len()),
            );
        }
    }
    Ok(lease)
}

/// `Dhcpcd.get_newest_lease`.
pub fn newest_lease(
    interface: &str,
    log: &mut ci_log::Logger,
) -> Result<Object, Error> {
    let output = subp::Subp::new([CLIENT_NAME, "--dumplease", "--ipv4only", interface])
        .check()
        .map_err(|error| {
            log.debug(SOURCE, &format!("dhcpcd exited with: {error}"));
            Error::NoLease(error.to_string())
        })?;
    let packet = std::fs::read(lease_packet_path(interface)).ok();
    parse_lease(&output.stdout_lossy(), interface, packet.as_deref(), log)
}

/// `Dhcpcd.dhcp_discovery`, without the process-group cleanup.
///
/// Upstream lets `dhcpcd` daemonise because `--dumplease` is the only way to
/// read what it got, then hunts down the pid and kills the group. This port
/// leaves the daemon running with `--persistent` already keeping the address,
/// which is the same end state minus the race upstream sleeps 300 seconds over
/// (deviation 112).
pub fn discover(interface: &str, log: &mut ci_log::Logger) -> Result<Object, Error> {
    log.debug(
        SOURCE,
        &format!("Performing a dhcp discovery on {interface}"),
    );
    if subp::which(CLIENT_NAME).is_none() {
        log.error(SOURCE, "dhcpcd executable not found");
        return Err(Error::MissingClient);
    }

    // dhcpcd sends discovery on the link itself, and `--script=/bin/true`
    // disables the hook that would otherwise have raised it.
    let _ = subp::Subp::new(["ip", "link", "set", "dev", interface, "up"]).check();

    let run = subp::Subp::new([
        CLIENT_NAME,
        "--ipv4only",
        "--waitip",
        "--persistent",
        "--noarp",
        "--debug",
        "--script=/bin/true",
        interface,
    ])
    .timeout(Some(TIMEOUT))
    .check();
    if let Err(error) = run {
        log.debug(SOURCE, &format!("dhcpcd exited with: {error}"));
        return Err(Error::NoLease(error.to_string()));
    }
    newest_lease(interface, log)
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

    fn log() -> ci_log::Logger {
        ci_log::Logger::silent()
    }

    /// A packet whose vendor area holds `code`/`value` at the very start.
    fn packet(code: u8, value: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 240];
        data.push(code);
        data.push(u8::try_from(value.len()).unwrap());
        data.extend_from_slice(value);
        data
    }

    const DUMP: &str = "broadcast_address='192.168.15.255'\n\
         dhcp_lease_time='3600'\n\
         ip_address='192.168.0.212'\n\
         routers='192.168.0.1'\n\
         subnet_mask='255.255.240.0'\n";

    #[test]
    fn the_dump_becomes_a_lease_with_dhclient_names() {
        let lease = parse_lease(DUMP, "eth0", None, &mut log()).unwrap();
        assert_eq!(lease["broadcast-address"], "192.168.15.255");
        assert_eq!(lease["interface"], "eth0");
        assert_eq!(lease["fixed-address"], "192.168.0.212");
        assert!(!lease.contains_key("ip-address"));
    }

    #[test]
    fn the_renamed_keys_move_to_the_end() {
        let dump = format!("{DUMP}classless_static_routes='0.0.0.0/0 10.0.0.1'\n");
        let lease = parse_lease(&dump, "eth0", None, &mut log()).unwrap();
        let keys: Vec<&str> = lease.keys().map(String::as_str).collect();
        assert_eq!(keys.last(), Some(&"static_routes"));
        assert_eq!(keys[keys.len() - 2], "fixed-address");
    }

    #[test]
    fn a_dump_with_no_assignments_is_not_a_lease() {
        assert!(matches!(
            parse_lease("no equals here\n", "eth0", None, &mut log()),
            Err(Error::InvalidLease(_))
        ));
    }

    #[test]
    fn option_245_is_the_wireserver_address() {
        let data = packet(245, &[168, 63, 129, 16]);
        let lease = parse_lease(DUMP, "eth0", Some(&data), &mut log()).unwrap();
        assert_eq!(lease["unknown-245"], "168.63.129.16");
    }

    #[test]
    fn an_option_245_that_is_not_an_address_is_refused_rather_than_raised() {
        let data = packet(245, &[168, 63]);
        let lease = parse_lease(DUMP, "eth0", Some(&data), &mut log()).unwrap();
        assert!(!lease.contains_key("unknown-245"));
    }

    #[test]
    fn options_before_245_are_stepped_over() {
        let mut data = packet(53, &[5]);
        data.extend_from_slice(&[245, 4, 10, 0, 0, 1]);
        assert_eq!(unknown_option(&data, 245), Some(&[10u8, 0, 0, 1][..]));
    }

    #[test]
    fn a_truncated_option_does_not_run_off_the_packet() {
        let mut data = vec![0u8; 240];
        data.extend_from_slice(&[245, 8, 10, 0]);
        assert_eq!(unknown_option(&data, 245), Some(&[10u8, 0][..]));
    }

    #[test]
    fn routes_pair_up_and_a_lone_destination_is_dropped() {
        let routes = parse_static_routes(
            "0.0.0.0/0 10.0.0.1 168.63.129.16/32 10.0.0.1 1.2.3.4/32",
            &mut log(),
        );
        assert_eq!(
            routes,
            vec![
                ("0.0.0.0/0".to_owned(), "10.0.0.1".to_owned()),
                ("168.63.129.16/32".to_owned(), "10.0.0.1".to_owned()),
            ]
        );
    }

    #[test]
    fn no_routes_at_all_is_a_warning_and_an_empty_list() {
        assert!(parse_static_routes("   ", &mut log()).is_empty());
    }
}
