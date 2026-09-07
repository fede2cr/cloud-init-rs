//! `cloudinit.netinfo.netdev_info`: what addresses the kernel currently has.
//!
//! Only the `ip --json addr` path is ported. Upstream keeps three more —
//! `ip addr show` text parsing for versions of iproute2 without `--json`,
//! `ifconfig` for net-tools systems, and a NetBSD variant — and none of them
//! can be reached on a machine this port supports.
//!
//! The one caller is [`crate::ephemeral`], which needs to know whether an
//! interface was already up and already had the address a lease is about to
//! hand it, so that tearing the lease down again does not remove
//! configuration that was there first.

use std::collections::BTreeMap;

use ci_sys::subp::{self, Subp};

/// One IPv4 address on an interface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Address {
    pub ip: String,
    pub mask: String,
    pub bcast: String,
    pub scope: String,
}

/// One interface, as much of it as the ephemeral path needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub hwaddr: String,
    pub up: bool,
    pub ipv4: Vec<Address>,
}

/// `netdev_info()`.
///
/// A machine without `ip`, or one where it fails, reports nothing. Upstream
/// would fall through to `ifconfig`; here an empty map simply means the
/// ephemeral bring-up assumes the interface was down and unaddressed, which is
/// the conservative answer — it queues the teardown rather than skipping it.
#[must_use]
pub fn netdev_info() -> BTreeMap<String, Device> {
    if subp::which("ip").is_none() {
        return BTreeMap::new();
    }
    let Ok(output) = Subp::new(["ip", "--json", "addr"]).run() else {
        return BTreeMap::new();
    };
    if !output.success() {
        return BTreeMap::new();
    }
    parse_iproute_json(&output.stdout_lossy())
}

/// `_netdev_info_iproute_json`, restricted to the fields above.
#[must_use]
pub fn parse_iproute_json(text: &str) -> BTreeMap<String, Device> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(text) else {
        return BTreeMap::new();
    };
    let mut devices = BTreeMap::new();
    for entry in entries {
        let Some(name) = entry.get("ifname").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let flags: Vec<&str> = entry
            .get("flags")
            .and_then(serde_json::Value::as_array)
            .map(|flags| flags.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        // Upstream only takes `address` as a hwaddr for ethernet links; a
        // tunnel's `address` is an IP and would be nonsense here.
        let hwaddr = if entry.get("link_type").and_then(serde_json::Value::as_str)
            == Some("ether")
        {
            entry
                .get("address")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        } else {
            ""
        };

        let mut device = Device {
            hwaddr: hwaddr.to_owned(),
            up: flags.contains(&"UP") && flags.contains(&"LOWER_UP"),
            ipv4: Vec::new(),
        };
        for addr in entry
            .get("addr_info")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if addr.get("family").and_then(serde_json::Value::as_str) != Some("inet") {
                continue;
            }
            let mask = addr
                .get("prefixlen")
                .and_then(serde_json::Value::as_u64)
                .and_then(|prefix| u8::try_from(prefix).ok())
                .map(crate::ip::net_prefix_to_ipv4_mask)
                .unwrap_or_default();
            device.ipv4.push(Address {
                ip: string_at(addr, "local"),
                mask,
                bcast: string_at(addr, "broadcast"),
                scope: string_at(addr, "scope"),
            });
        }
        devices.insert(name.to_owned(), device);
    }
    devices
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
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

    const SAMPLE: &str = r#"[
      {"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"link_type":"loopback",
       "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
      {"ifname":"eth0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],
       "link_type":"ether","address":"00:0d:3a:11:22:33",
       "addr_info":[{"family":"inet","local":"10.0.0.4","prefixlen":24,
                     "broadcast":"10.0.0.255","scope":"global"},
                    {"family":"inet6","local":"fe80::1","prefixlen":64,"scope":"link"}]},
      {"ifname":"eth1","flags":["BROADCAST","MULTICAST"],"link_type":"ether",
       "address":"00:0d:3a:44:55:66","addr_info":[]}
    ]"#;

    #[test]
    fn an_interface_that_is_up_with_an_address_is_reported_in_full() {
        let devices = parse_iproute_json(SAMPLE);
        let eth0 = &devices["eth0"];
        assert!(eth0.up);
        assert_eq!(eth0.hwaddr, "00:0d:3a:11:22:33");
        assert_eq!(eth0.ipv4.len(), 1, "the ipv6 address must not be counted");
        assert_eq!(eth0.ipv4[0].ip, "10.0.0.4");
        assert_eq!(eth0.ipv4[0].mask, "255.255.255.0");
        assert_eq!(eth0.ipv4[0].bcast, "10.0.0.255");
        assert_eq!(eth0.ipv4[0].scope, "global");
    }

    #[test]
    fn up_needs_both_up_and_lower_up() {
        let devices = parse_iproute_json(SAMPLE);
        assert!(!devices["eth1"].up);
        assert!(devices["lo"].up);
    }

    #[test]
    fn a_loopback_link_has_no_hardware_address() {
        assert_eq!(parse_iproute_json(SAMPLE)["lo"].hwaddr, "");
    }

    #[test]
    fn output_that_is_not_a_json_array_is_no_devices() {
        assert!(parse_iproute_json("").is_empty());
        assert!(parse_iproute_json("{}").is_empty());
        assert!(parse_iproute_json("not json").is_empty());
    }
}
