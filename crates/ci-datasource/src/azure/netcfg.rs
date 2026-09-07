//! Port of the network half of `sources/DataSourceAzure.py`: turning IMDS
//! network metadata into a netplan v2 document.
//!
//! Enumerating the host's interfaces is `cloudinit.net.get_interfaces`, which
//! belongs with the network layer, so the interface list is an argument here.

use ci_config::{Object, Value};

const SOURCE: &str = "DataSourceAzure.py";

/// One entry of `net.get_interfaces()`, narrowed to the two fields the Azure
/// datasource reads off it.
#[derive(Debug, Clone)]
pub struct Interface {
    pub mac: String,
    pub driver: Option<String>,
}

/// `normalize_mac_address`.
#[must_use]
pub fn normalize_mac_address(mac: &str) -> String {
    let chars: Vec<char> = mac.chars().collect();
    if chars.len() != 12 {
        return mac.to_lowercase();
    }
    chars
        .chunks(2)
        .map(|pair| pair.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(":")
        .to_lowercase()
}

/// `get_hv_netvsc_macs_normalized`.
#[must_use]
pub fn hv_netvsc_macs_normalized(interfaces: &[Interface]) -> Vec<String> {
    interfaces
        .iter()
        .filter(|nic| nic.driver.as_deref() == Some("hv_netvsc"))
        .map(|nic| normalize_mac_address(&nic.mac))
        .collect()
}

/// `determine_device_driver_for_mac`.
#[must_use]
pub fn determine_device_driver_for_mac(
    mac: &str,
    interfaces: &[Interface],
    log: &mut ci_log::Logger,
) -> Option<String> {
    let drivers: Vec<Option<&str>> = interfaces
        .iter()
        .filter(|nic| mac == normalize_mac_address(&nic.mac))
        .map(|nic| nic.driver.as_deref())
        .collect();

    if drivers.contains(&Some("hv_netvsc")) {
        return Some("hv_netvsc".to_owned());
    }

    let rendered = py_list(&drivers);
    if drivers.len() == 1 {
        // A lone interface with no driver reported lands here too, and
        // upstream returns its `None` from this branch.
        log.debug(
            SOURCE,
            &format!("Assuming driver for interface with mac={mac} drivers={rendered}"),
        );
        return drivers.first().copied().flatten().map(ToOwned::to_owned);
    }

    log.warning(
        SOURCE,
        &format!(
            "Unable to specify driver for interface with mac={mac} drivers={rendered}"
        ),
    );
    None
}

/// `generate_network_config_from_instance_network_metadata`.
///
/// # Errors
/// The text of the Python exception the document would have raised, which the
/// caller logs before falling back to a generated config.
pub fn generate_network_config(
    network_metadata: &Object,
    apply_network_config_for_secondary_ips: bool,
    interfaces: &[Interface],
    log: &mut ci_log::Logger,
) -> Result<Value, String> {
    let list = network_metadata
        .get("interface")
        .ok_or_else(|| "'interface'".to_owned())?
        .as_array()
        .ok_or_else(|| "'interface' is not a list".to_owned())?;

    let mut ethernets = Object::new();
    for (idx, intf) in list.iter().enumerate() {
        let nicname = format!("eth{idx}");
        let dev_config =
            device_config(idx, intf, apply_network_config_for_secondary_ips, log)?;
        let Some(mut dev_config) = dev_config else {
            continue;
        };

        let mac = normalize_mac_address(
            intf.get("macAddress")
                .ok_or_else(|| "'macAddress'".to_owned())?
                .as_str()
                .ok_or_else(|| "macAddress is not a string".to_owned())?,
        );
        let mut matcher = Object::new();
        matcher.insert("macaddress".to_owned(), Value::from(mac.clone()));
        if let Some(driver) = determine_device_driver_for_mac(&mac, interfaces, log) {
            matcher.insert("driver".to_owned(), Value::from(driver));
        }
        dev_config.insert("match".to_owned(), Value::Object(matcher));
        dev_config.insert("set-name".to_owned(), Value::from(nicname.clone()));
        ethernets.insert(nicname, Value::Object(dev_config));
    }

    let mut netconfig = Object::new();
    netconfig.insert("version".to_owned(), Value::from(2));
    netconfig.insert("ethernets".to_owned(), Value::Object(ethernets));
    Ok(Value::Object(netconfig))
}

/// The body of the per-interface loop, up to the point where the match rules
/// are attached. `None` is an interface with no addresses at all, which
/// upstream logs and skips.
fn device_config(
    idx: usize,
    intf: &Value,
    apply_network_config_for_secondary_ips: bool,
    log: &mut ci_log::Logger,
) -> Result<Option<Object>, String> {
    let mut dhcp_override = Object::new();
    dhcp_override.insert("route-metric".to_owned(), Value::from((idx + 1) * 100));
    if idx > 0 {
        // Resolution through a secondary NIC is not supported.
        dhcp_override.insert("use-dns".to_owned(), Value::Bool(false));
    }

    let mut dev_config = Object::new();
    dev_config.insert("dhcp4".to_owned(), Value::Bool(true));
    dev_config.insert(
        "dhcp4-overrides".to_owned(),
        Value::Object(dhcp_override.clone()),
    );
    dev_config.insert("dhcp6".to_owned(), Value::Bool(false));

    let mut has_ip_address = false;
    for addr_type in ["ipv4", "ipv6"] {
        let addresses = intf
            .get(addr_type)
            .and_then(|family| family.get("ipAddress"))
            .and_then(Value::as_array)
            .filter(|addresses| !addresses.is_empty());
        let Some(addresses) = addresses else {
            log.debug(
                SOURCE,
                &format!("No {addr_type} addresses found for: {}", py_repr(intf)),
            );
            continue;
        };
        has_ip_address = true;

        let default_prefix = if addr_type == "ipv4" {
            "24"
        } else {
            dev_config.insert("dhcp6".to_owned(), Value::Bool(true));
            dev_config.insert(
                "dhcp6-overrides".to_owned(),
                Value::Object(dhcp_override.clone()),
            );
            "128"
        };

        if !apply_network_config_for_secondary_ips {
            continue;
        }

        for addr in addresses.iter().skip(1) {
            let subnet = intf
                .get(addr_type)
                .and_then(|family| family.get("subnet"))
                .ok_or_else(|| "'subnet'".to_owned())?
                .get(0)
                .ok_or_else(|| "list index out of range".to_owned())?;
            let prefix = subnet
                .get("prefix")
                .map_or_else(|| default_prefix.to_owned(), py_str);
            let private_ip = addr
                .get("privateIpAddress")
                .ok_or_else(|| "'privateIpAddress'".to_owned())?;
            let rendered = Value::from(format!("{}/{prefix}", py_str(private_ip)));
            if let Value::Array(list) = dev_config
                .entry("addresses")
                .or_insert_with(|| Value::Array(Vec::new()))
            {
                list.push(rendered);
            }
        }
    }

    if has_ip_address {
        return Ok(Some(dev_config));
    }
    log.debug(
        SOURCE,
        &format!(
            "No configuration for: eth{idx} (dev_config={}) (has_ip_address=False)",
            py_repr(&Value::Object(dev_config)),
        ),
    );
    Ok(None)
}

/// `validate_imds_network_metadata`.
///
/// `primary_mac` stands in for the ephemeral DHCP context's interface, which
/// upstream reads off itself; `None` covers both "no context" and "no mac".
#[must_use]
pub fn validate_imds_network_metadata(
    imds_md: &Object,
    interfaces: &[Interface],
    primary_mac: Option<&str>,
    log: &mut ci_log::Logger,
) -> bool {
    let local_macs = hv_netvsc_macs_normalized(interfaces);

    let network_config = imds_md.get("network");
    let imds_macs: Option<Vec<String>> = network_config
        .and_then(|network| network.get("interface"))
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .map(|intf| {
                    intf.get("macAddress")
                        .map(|mac| normalize_mac_address(&py_str(mac)))
                })
                .collect()
        });
    let Some(imds_macs) = imds_macs else {
        log.warning(
            SOURCE,
            &format!(
                "IMDS network metadata has incomplete configuration: {}",
                py_repr(network_config.unwrap_or(&Value::Null))
            ),
        );
        return false;
    };

    let missing: Vec<&String> = local_macs
        .iter()
        .filter(|mac| !imds_macs.contains(mac))
        .collect();
    if missing.is_empty() {
        return true;
    }

    let rendered = py_list(
        &missing
            .iter()
            .map(|mac| Some(mac.as_str()))
            .collect::<Vec<_>>(),
    );
    let network_config = network_config.unwrap_or(&Value::Null);
    log.warning(
        SOURCE,
        &format!(
            "IMDS network metadata is missing configuration for NICs {rendered}: {}",
            py_repr(network_config)
        ),
    );

    let Some(primary_mac) = primary_mac.map(normalize_mac_address) else {
        return false;
    };
    if missing.iter().any(|mac| **mac == primary_mac) {
        log.warning(
            SOURCE,
            &format!(
                "IMDS network metadata is missing primary NIC '{primary_mac}': {}",
                py_repr(network_config)
            ),
        );
    }
    false
}

/// `str()` of a JSON value, as an f-string would render it.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

/// `repr()` of a value, far enough for the diagnostic lines above.
fn py_repr(value: &Value) -> String {
    match value {
        Value::String(text) => format!("'{}'", text.replace('\'', "\\'")),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(fields) => {
            let rendered: Vec<String> = fields
                .iter()
                .map(|(key, value)| format!("'{key}': {}", py_repr(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
        other => py_str(other),
    }
}

/// `repr()` of a list of optional strings, which is what the driver
/// diagnostics print.
fn py_list(items: &[Option<&str>]) -> String {
    let rendered: Vec<String> = items
        .iter()
        .map(|item| {
            item.map_or_else(
                || "None".to_owned(),
                |text| format!("'{}'", text.replace('\'', "\\'")),
            )
        })
        .collect();
    format!("[{}]", rendered.join(", "))
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

    fn obj(json: &str) -> Object {
        serde_json::from_str(json).unwrap()
    }

    fn nic(mac: &str, driver: &str) -> Interface {
        Interface {
            mac: mac.to_owned(),
            driver: Some(driver.to_owned()),
        }
    }

    #[test]
    fn a_bare_twelve_character_mac_gains_colons() {
        assert_eq!(normalize_mac_address("001122AABBCC"), "00:11:22:aa:bb:cc");
        assert_eq!(
            normalize_mac_address("00:11:22:AA:BB:CC"),
            "00:11:22:aa:bb:cc"
        );
        assert_eq!(normalize_mac_address("short"), "short");
    }

    #[test]
    fn the_first_address_is_left_to_dhcp_and_the_rest_are_static() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [{"macAddress": "001122AABBCC",
                "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                                       {"privateIpAddress": "10.0.0.5"}],
                         "subnet": [{"prefix": "16"}]}}]}"#);

        let config = generate_network_config(&md, true, &[], &mut log).unwrap();

        let eth0 = &config["ethernets"]["eth0"];
        assert_eq!(config["version"], Value::from(2));
        assert_eq!(eth0["dhcp4"], Value::Bool(true));
        assert_eq!(eth0["dhcp6"], Value::Bool(false));
        assert_eq!(eth0["addresses"], Value::from(vec!["10.0.0.5/16"]));
        assert_eq!(
            eth0["match"]["macaddress"],
            Value::from("00:11:22:aa:bb:cc")
        );
        assert_eq!(eth0["set-name"], Value::from("eth0"));
        assert_eq!(eth0["dhcp4-overrides"]["route-metric"], Value::from(100));
    }

    #[test]
    fn a_secondary_nic_costs_more_and_gives_up_dns() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [
                {"macAddress": "AA", "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"}]}},
                {"macAddress": "BB", "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.1.4"}]}}]}"#);

        let config = generate_network_config(&md, true, &[], &mut log).unwrap();

        let overrides = &config["ethernets"]["eth1"]["dhcp4-overrides"];
        assert_eq!(overrides["route-metric"], Value::from(200));
        assert_eq!(overrides["use-dns"], Value::Bool(false));
        assert!(config["ethernets"]["eth0"]["dhcp4-overrides"]
            .get("use-dns")
            .is_none());
    }

    #[test]
    fn an_ipv6_address_turns_on_dhcp6_with_the_same_overrides() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [{"macAddress": "AA",
                "ipv6": {"ipAddress": [{"privateIpAddress": "fd00::4"},
                                       {"privateIpAddress": "fd00::5"}],
                         "subnet": [{}]}}]}"#);

        let config = generate_network_config(&md, true, &[], &mut log).unwrap();

        let eth0 = &config["ethernets"]["eth0"];
        assert_eq!(eth0["dhcp6"], Value::Bool(true));
        assert_eq!(eth0["dhcp6-overrides"]["route-metric"], Value::from(100));
        // No prefix in the subnet, so the IPv6 default applies.
        assert_eq!(eth0["addresses"], Value::from(vec!["fd00::5/128"]));
    }

    #[test]
    fn an_interface_without_addresses_is_left_out_entirely() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [{"macAddress": "AA", "ipv4": {}}]}"#);

        let config = generate_network_config(&md, true, &[], &mut log).unwrap();

        assert_eq!(config["ethernets"], Value::Object(Object::new()));
    }

    #[test]
    fn secondary_addresses_are_dropped_when_they_are_turned_off() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [{"macAddress": "AA",
                "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                                       {"privateIpAddress": "10.0.0.5"}],
                         "subnet": [{"prefix": "16"}]}}]}"#);

        let config = generate_network_config(&md, false, &[], &mut log).unwrap();

        assert!(config["ethernets"]["eth0"].get("addresses").is_none());
    }

    #[test]
    fn a_missing_subnet_is_the_key_error_upstream_reports() {
        let mut log = ci_log::Logger::silent();
        let md = obj(r#"{"interface": [{"macAddress": "AA",
                "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                                       {"privateIpAddress": "10.0.0.5"}]}}]}"#);

        let error = generate_network_config(&md, true, &[], &mut log);

        assert_eq!(error, Err("'subnet'".to_owned()));
    }

    #[test]
    fn the_synthetic_hyperv_driver_wins_over_whatever_else_shares_the_mac() {
        let mut log = ci_log::Logger::silent();
        let nics = [
            nic("00:11:22:aa:bb:cc", "mlx5_core"),
            nic("001122AABBCC", "hv_netvsc"),
        ];

        let driver =
            determine_device_driver_for_mac("00:11:22:aa:bb:cc", &nics, &mut log);

        assert_eq!(driver.as_deref(), Some("hv_netvsc"));
    }

    #[test]
    fn a_mac_claimed_by_two_drivers_gets_none() {
        let mut log = ci_log::Logger::silent();
        let nics = [nic("AA:BB", "mlx5_core"), nic("AA:BB", "ixgbevf")];

        assert_eq!(
            determine_device_driver_for_mac("aa:bb", &nics, &mut log),
            None
        );
        assert_eq!(
            determine_device_driver_for_mac("aa:bb", &nics[..1], &mut log).as_deref(),
            Some("mlx5_core")
        );
        assert_eq!(
            determine_device_driver_for_mac("aa:bb", &[], &mut log),
            None
        );
    }

    #[test]
    fn metadata_that_covers_every_synthetic_nic_validates() {
        let mut log = ci_log::Logger::silent();
        let nics = [nic("00:11:22:aa:bb:cc", "hv_netvsc")];
        let good =
            obj(r#"{"network": {"interface": [{"macAddress": "001122AABBCC"}]}}"#);
        let missing =
            obj(r#"{"network": {"interface": [{"macAddress": "DDEEFF001122"}]}}"#);
        let broken = obj(r#"{"network": {}}"#);

        assert!(validate_imds_network_metadata(&good, &nics, None, &mut log));
        assert!(!validate_imds_network_metadata(
            &missing, &nics, None, &mut log
        ));
        assert!(!validate_imds_network_metadata(
            &broken, &nics, None, &mut log
        ));
        // Nothing local to be missing, so nothing to complain about.
        assert!(validate_imds_network_metadata(
            &missing,
            &[],
            None,
            &mut log
        ));
    }
}
