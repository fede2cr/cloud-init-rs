//! The subset of Python's `ipaddress` that `cloudinit.net` leans on.
//!
//! `network_state` and the renderers make decisions by *asking whether a string
//! parses* — `is_ipv4_address`, `is_ip_network`, and so on — so the parser's
//! strictness is part of the output, not an implementation detail. Rust's own
//! `Ipv4Addr`/`Ipv6Addr` parsers agree with modern `CPython` on the case that
//! matters most (leading zeros in an octet are rejected, so `010.0.0.1` is not
//! an address in either), which is why they are used underneath rather than a
//! hand-rolled scanner.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// `ipaddress.IPv4Address`.
#[must_use]
pub fn is_ipv4_address(address: &str) -> bool {
    address.parse::<Ipv4Addr>().is_ok()
}

/// `ipaddress.IPv6Address`.
#[must_use]
pub fn is_ipv6_address(address: &str) -> bool {
    address.parse::<Ipv6Addr>().is_ok()
}

/// `ipaddress.ip_address`.
#[must_use]
pub fn is_ip_address(address: &str) -> bool {
    address.parse::<IpAddr>().is_ok()
}

/// `ipaddress.IPv4Network(..., strict=False)`.
#[must_use]
pub fn is_ipv4_network(address: &str) -> bool {
    parse_ipv4_network(address).is_some()
}

/// `ipaddress.IPv6Network(..., strict=False)`.
#[must_use]
pub fn is_ipv6_network(address: &str) -> bool {
    parse_ipv6_network(address).is_some()
}

/// `ipaddress.ip_network(..., strict=False)`.
#[must_use]
pub fn is_ip_network(address: &str) -> bool {
    is_ipv4_network(address) || is_ipv6_network(address)
}

/// The address and prefix of an `a.b.c.d[/mask]`, host bits allowed.
#[must_use]
pub fn parse_ipv4_network(address: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, mask) = split_once_slash(address);
    let ip: Ipv4Addr = addr.parse().ok()?;
    let prefix = match mask {
        Some(m) => ipv4_mask_to_net_prefix(m)?,
        None => 32,
    };
    Some((ip, prefix))
}

/// The address and prefix of an `addr[/prefix]`, host bits allowed.
#[must_use]
pub fn parse_ipv6_network(address: &str) -> Option<(Ipv6Addr, u8)> {
    let (addr, mask) = split_once_slash(address);
    let ip: Ipv6Addr = addr.parse().ok()?;
    let prefix = match mask {
        Some(m) => ipv6_mask_to_net_prefix(m)?,
        None => 128,
    };
    Some((ip, prefix))
}

/// `ipaddress.ip_network` splits on the *first* slash and rejects the rest.
fn split_once_slash(address: &str) -> (&str, Option<&str>) {
    match address.split_once('/') {
        Some((addr, mask)) => (addr, Some(mask)),
        None => (address, None),
    }
}

/// `ipv4_mask_to_net_prefix`: a prefix length, a netmask, or a hostmask.
///
/// The hostmask spelling (`0.0.0.255` meaning /24) is Python's, and it reaches
/// here from user-supplied `netmask:` keys, so it is not optional.
#[must_use]
pub fn ipv4_mask_to_net_prefix(mask: &str) -> Option<u8> {
    if let Some(prefix) = parse_prefix_len(mask, 32) {
        return Some(prefix);
    }
    let bits = u32::from(mask.parse::<Ipv4Addr>().ok()?);
    contiguous_ones(u128::from(bits), 32)
        // A hostmask is the complement of the netmask: `0.0.0.255` is /24.
        .or_else(|| contiguous_ones(u128::from(!bits), 32))
}

/// `ipv6_mask_to_net_prefix`: a prefix length, or the very uncommon netmask.
#[must_use]
pub fn ipv6_mask_to_net_prefix(mask: &str) -> Option<u8> {
    if let Some(prefix) = parse_prefix_len(mask, 128) {
        return Some(prefix);
    }
    let bits = u128::from(mask.parse::<Ipv6Addr>().ok()?);
    contiguous_ones(bits, 128)
}

/// A decimal prefix length, with Python's rejection of `+`, spaces and
/// leading zeros beyond a bare `0`.
fn parse_prefix_len(mask: &str, max: u8) -> Option<u8> {
    if mask.is_empty() || !mask.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if mask.len() > 1 && mask.starts_with('0') {
        return None;
    }
    let value: u8 = mask.parse().ok()?;
    (value <= max).then_some(value)
}

/// The prefix length, if the value is a contiguous `1*0*` netmask.
fn contiguous_ones(bits: u128, width: u8) -> Option<u8> {
    (0..=width).find(|&prefix| mask_bits(prefix, width) == bits)
}

/// `1^prefix` followed by zeros, in a `width`-bit space.
fn mask_bits(prefix: u8, width: u8) -> u128 {
    if prefix == 0 {
        return 0;
    }
    let ones = (!0u128) >> (128 - u32::from(prefix));
    ones << (width - prefix)
}

/// `net_prefix_to_ipv4_mask`.
#[must_use]
pub fn net_prefix_to_ipv4_mask(prefix: u8) -> String {
    let bits: u32 = if prefix == 0 {
        0
    } else {
        (!0u32) << (32 - u32::from(prefix.min(32)))
    };
    Ipv4Addr::from(bits).to_string()
}

/// `is_ip_in_subnet`. `None` where Python raises `ValueError`, which the one
/// caller (`should_add_gateway_onlink_flag`) turns into `false` plus a warning.
#[must_use]
pub fn is_ip_in_subnet(address: &str, subnet: &str) -> Option<bool> {
    let ip: IpAddr = address.parse().ok()?;
    match ip {
        IpAddr::V4(v4) => {
            let (net, prefix) = parse_ipv4_network(subnet)?;
            Some(masked_v4(v4, prefix) == masked_v4(net, prefix))
        }
        IpAddr::V6(v6) => {
            let (net, prefix) = parse_ipv6_network(subnet)?;
            Some(masked_v6(v6, prefix) == masked_v6(net, prefix))
        }
    }
}

fn masked_v4(addr: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(addr);
    if prefix == 0 {
        0
    } else {
        bits & ((!0u32) << (32 - u32::from(prefix.min(32))))
    }
}

fn masked_v6(addr: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(addr);
    if prefix == 0 {
        0
    } else {
        bits & ((!0u128) << (128 - u32::from(prefix.min(128))))
    }
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

    #[test]
    fn masks_and_prefixes_round_trip() {
        assert_eq!(ipv4_mask_to_net_prefix("24"), Some(24));
        assert_eq!(ipv4_mask_to_net_prefix("255.255.255.0"), Some(24));
        assert_eq!(ipv4_mask_to_net_prefix("0.0.0.255"), Some(24));
        assert_eq!(ipv4_mask_to_net_prefix("0.0.0.0"), Some(0));
        assert_eq!(ipv4_mask_to_net_prefix("255.255.255.255"), Some(32));
        assert_eq!(ipv4_mask_to_net_prefix("255.255.0.255"), None);
        assert_eq!(ipv4_mask_to_net_prefix("33"), None);
        assert_eq!(net_prefix_to_ipv4_mask(24), "255.255.255.0");
        assert_eq!(net_prefix_to_ipv4_mask(0), "0.0.0.0");
        assert_eq!(net_prefix_to_ipv4_mask(32), "255.255.255.255");
    }

    #[test]
    fn ipv6_masks() {
        assert_eq!(ipv6_mask_to_net_prefix("64"), Some(64));
        assert_eq!(ipv6_mask_to_net_prefix("ffff:ffff:ffff::"), Some(48));
        assert_eq!(ipv6_mask_to_net_prefix("::"), Some(0));
        assert_eq!(ipv6_mask_to_net_prefix("ffff:0:ffff::"), None);
    }

    #[test]
    fn networks_allow_host_bits() {
        assert!(is_ipv4_network("192.168.1.5/24"));
        assert!(is_ipv4_network("192.168.1.0"));
        assert!(is_ipv6_network("2001:db8::5/64"));
        assert!(!is_ipv4_network("192.168.1.0/33"));
        assert!(!is_ip_network("not-an-address"));
    }

    #[test]
    fn leading_zeros_are_not_addresses() {
        assert!(!is_ipv4_address("010.0.0.1"));
        assert!(is_ipv4_address("10.0.0.1"));
    }

    #[test]
    fn subnet_containment() {
        assert_eq!(is_ip_in_subnet("192.168.1.1", "192.168.1.5/24"), Some(true));
        assert_eq!(is_ip_in_subnet("10.0.0.1", "192.168.1.5/24"), Some(false));
        assert_eq!(is_ip_in_subnet("::1", "::/0"), Some(true));
        assert_eq!(is_ip_in_subnet("192.168.1.1", "garbage"), None);
    }
}
