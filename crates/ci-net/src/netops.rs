//! `cloudinit.net.netops.iproute2`: the `ip` invocations that change a live
//! machine.
//!
//! Only the calls [`crate::ephemeral`] makes are ported. Every one of them is a
//! literal argv, spelled out so it can be read against upstream side by side —
//! there is no differential harness for a module whose whole effect is on the
//! kernel's routing table.

use ci_sys::subp::{self, Subp};

/// A command that changed, or failed to change, the machine.
///
/// `argv` is kept unjoined because [`crate::activators`] has to reproduce
/// upstream's `"Running interface command %s failed" % e.cmd`, and `e.cmd` is
/// a Python list.
#[derive(Debug)]
pub struct Error {
    pub argv: Vec<String>,
    pub reason: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` failed: {}", self.argv.join(" "), self.reason)
    }
}

fn run(argv: &[&str]) -> Result<String, Error> {
    let owned = || argv.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let output = Subp::new(argv).run().map_err(|error| Error {
        argv: owned(),
        reason: error.to_string(),
    })?;
    if output.success() {
        return Ok(output.stdout_lossy().into_owned());
    }
    Err(Error {
        argv: owned(),
        reason: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Whether `ip` is on `PATH` at all.
#[must_use]
pub fn available() -> bool {
    subp::which("ip").is_some()
}

/// `Iproute2.link_up(interface, family="inet")`.
pub fn link_up(interface: &str, family: Option<&str>) -> Result<(), Error> {
    match family {
        Some(family) => run(&[
            "ip", "-family", family, "link", "set", "dev", interface, "up",
        ]),
        None => run(&["ip", "link", "set", "dev", interface, "up"]),
    }
    .map(drop)
}

/// `Iproute2.link_down`.
pub fn link_down(interface: &str, family: Option<&str>) -> Result<(), Error> {
    match family {
        Some(family) => run(&[
            "ip", "-family", family, "link", "set", "dev", interface, "down",
        ]),
        None => run(&["ip", "link", "set", "dev", interface, "down"]),
    }
    .map(drop)
}

/// `Iproute2.add_addr`.
pub fn add_addr(
    interface: &str,
    address: &str,
    broadcast: Option<&str>,
) -> Result<(), Error> {
    let mut argv = vec!["ip", "-family", "inet", "addr", "add", address];
    if let Some(broadcast) = broadcast {
        argv.extend(["broadcast", broadcast]);
    }
    argv.extend(["dev", interface]);
    run(&argv).map(drop)
}

/// `Iproute2.del_addr`.
pub fn del_addr(interface: &str, address: &str) -> Result<(), Error> {
    run(&[
        "ip", "-family", "inet", "addr", "del", address, "dev", interface,
    ])
    .map(drop)
}

/// `Iproute2.add_route`, which is `ip route replace`.
///
/// A gateway of `0.0.0.0` is dropped rather than passed as `via`, exactly as
/// upstream — it is how a DHCP server spells "on-link".
pub fn add_route(
    interface: &str,
    route: &str,
    gateway: Option<&str>,
    source_address: Option<&str>,
) -> Result<(), Error> {
    let mut argv = vec!["ip", "-4", "route", "replace", route];
    if let Some(gateway) = gateway.filter(|g| !g.is_empty() && *g != "0.0.0.0") {
        argv.extend(["via", gateway]);
    }
    argv.extend(["dev", interface]);
    if let Some(source) = source_address {
        argv.extend(["src", source]);
    }
    run(&argv).map(drop)
}

/// `Iproute2.append_route`.
///
/// `append`, not `add`: rfc3442 classless static routes may name the same
/// subnet twice through different routers, and `ip route add` refuses the
/// second one.
pub fn append_route(
    interface: &str,
    address: &str,
    gateway: &str,
) -> Result<(), Error> {
    let mut argv = vec!["ip", "-4", "route", "append", address];
    if !gateway.is_empty() && gateway != "0.0.0.0" {
        argv.extend(["via", gateway]);
    }
    argv.extend(["dev", interface]);
    run(&argv).map(drop)
}

/// `Iproute2.del_route`.
pub fn del_route(
    interface: &str,
    address: &str,
    gateway: Option<&str>,
    source_address: Option<&str>,
) -> Result<(), Error> {
    let mut argv = vec!["ip", "-4", "route", "del", address];
    if let Some(gateway) = gateway.filter(|g| !g.is_empty() && *g != "0.0.0.0") {
        argv.extend(["via", gateway]);
    }
    argv.extend(["dev", interface]);
    if let Some(source) = source_address {
        argv.extend(["src", source]);
    }
    run(&argv).map(drop)
}

/// `Iproute2.get_default_route`.
pub fn default_route() -> Result<String, Error> {
    run(&["ip", "route", "show", "0.0.0.0/0"])
}
