//! `cloudinit.net.netplan`: the only renderer this port targets so far.
//!
//! Netplan is the renderer every distro cloud-init-rs cares about actually
//! uses, and it is the one with the passthrough shortcut — a v2 config is
//! written back out almost verbatim. That makes it both the cheapest renderer
//! to be correct about and the one whose output is most load-bearing.

use serde_json::{json, Map, Value};

use crate::ip;
use crate::state::{NetworkState, BOND_V1_TO_V2, BRIDGE_V1_TO_V2, IPV6_DYNAMIC_TYPES};

/// `CLOUDINIT_NETPLAN_FILE`.
pub const NETPLAN_FILE: &str = "/etc/netplan/50-cloud-init.yaml";

/// `KNOWN_SNAPD_CONFIG`, the header net-convert renders with.
pub const NETPLAN_HEADER: &str = "\
# This file is generated from information provided by the datasource.  Changes
# to it will not persist across an instance reboot.  To disable cloud-init's
# network configuration capabilities, write a file
# /etc/cloud/cloud.cfg.d/99-disable-network-config.cfg with the following:
# network: {config: disabled}
";

/// The renderer feature flags `_extract_addresses` consults. `net-convert`
/// hardcodes both.
#[derive(Debug, Clone, Copy)]
pub struct Features {
    pub dhcp_use_domains: bool,
    pub ipv6_mtu: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            dhcp_use_domains: true,
            ipv6_mtu: true,
        }
    }
}

/// Warnings emitted while rendering, so a caller can log them as upstream does.
#[derive(Debug, Default, Clone)]
pub struct Warnings(pub Vec<String>);

/// `Renderer._render_content`.
#[must_use]
pub fn render_content(
    state: &NetworkState,
    features: Features,
    warnings: &mut Warnings,
) -> String {
    if state.version() == 2 && state.is_passthrough() {
        // "V2 to V2 passthrough": the config is already netplan's own format.
        return ci_core::yamlfmt::dumps_bare(&json!({"network": state.config()}));
    }

    let mut ethernets = Map::new();
    let mut wifis = Map::new();
    let mut bridges = Map::new();
    let mut bonds = Map::new();
    let mut vlans = Map::new();

    for iface in state.interfaces() {
        // `None` is dropped, but `False` is kept: `accept-ra: false` is a
        // meaningful netplan setting, `mtu: None` is not.
        let ifcfg: Map<String, Value> = iface
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let ifname = ifcfg
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let kind = ifcfg.get("type").and_then(Value::as_str).unwrap_or("");
        let mut entry = Map::new();

        match kind {
            "physical" => {
                render_physical(&ifcfg, &mut entry, &ifname);
                extract_addresses(&ifcfg, &mut entry, &ifname, features, warnings);
                ethernets.insert(ifname.clone(), Value::Object(entry));
            }
            "bond" => {
                render_bond(state, &ifcfg, &mut entry, &ifname);
                extract_addresses(&ifcfg, &mut entry, &ifname, features, warnings);
                bonds.insert(ifname.clone(), Value::Object(entry));
            }
            "bridge" => {
                if !render_bridge(&ifcfg, &mut entry, warnings) {
                    continue;
                }
                extract_addresses(&ifcfg, &mut entry, &ifname, features, warnings);
                bridges.insert(ifname.clone(), Value::Object(entry));
            }
            "vlan" => {
                render_vlan(&ifcfg, &mut entry);
                extract_addresses(&ifcfg, &mut entry, &ifname, features, warnings);
                vlans.insert(ifname.clone(), Value::Object(entry));
            }
            _ => {}
        }
    }

    // Global DNS is not a netplan concept, so it is pushed onto every device
    // that already has addresses of its own.
    let nameservers = state.dns_nameservers();
    let searchdomains = state.dns_searchdomains();
    if !nameservers.is_empty() || !searchdomains.is_empty() {
        for section in [
            &mut ethernets,
            &mut wifis,
            &mut bridges,
            &mut bonds,
            &mut vlans,
        ] {
            for cfg in section.values_mut() {
                if cfg.get("addresses").is_none() || cfg.get("nameservers").is_some() {
                    continue;
                }
                cfg["nameservers"] = json!({
                    "addresses": nameservers,
                    "search": searchdomains,
                });
            }
        }
    }

    let mut out = String::from("network:\n    version: 2\n");
    for (name, section) in [
        ("ethernets", ethernets),
        ("wifis", wifis),
        ("bonds", bonds),
        ("bridges", bridges),
        ("vlans", vlans),
    ] {
        if section.is_empty() {
            continue;
        }
        let dump = ci_core::yamlfmt::dumps_bare(&json!({name: Value::Object(section)}));
        for line in dump.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// `Renderer.render_network_state`, minus the netplan CLI calls: the header is
/// prepended and the content returned ready to write.
#[must_use]
pub fn render_with_header(content: &str, header: &str) -> String {
    let mut header = header.to_owned();
    // An empty header still becomes a bare newline, so the file opens with a
    // blank line. That is upstream's output, odd as it looks.
    if !header.ends_with('\n') {
        header.push('\n');
    }
    format!("{header}{content}")
}

/// A `physical` device: netplan matches on the mac and renames, or on nothing
/// at all and keys off the name it was already given.
fn render_physical(
    ifcfg: &Map<String, Value>,
    entry: &mut Map<String, Value>,
    ifname: &str,
) {
    entry.insert("set-name".to_owned(), json!(ifname));
    if let Some(keep) = ifcfg.get("keep_configuration") {
        entry.insert("critical".to_owned(), keep.clone());
    }
    let mut matches = ifcfg.get("match").cloned();
    if matches.is_none() || matches == Some(Value::Null) {
        matches = ifcfg
            .get("mac_address")
            .and_then(Value::as_str)
            .map(|m| json!({"macaddress": m.to_lowercase()}));
    }
    match matches {
        Some(m) => {
            entry.insert("match".to_owned(), m);
        }
        // Nothing to match on: netplan keys off the device name.
        None => {
            entry.remove("set-name");
        }
    }
}

fn render_bond(
    state: &NetworkState,
    ifcfg: &Map<String, Value>,
    entry: &mut Map<String, Value>,
    ifname: &str,
) {
    if let Some(mac) = ifcfg.get("mac_address") {
        entry.insert("macaddress".to_owned(), mac.clone());
    }
    // v1 records "none" and hangs the membership off each slave instead.
    if ifcfg.get("bond-slaves").and_then(Value::as_str) == Some("none") {
        entry.insert(
            "interfaces".to_owned(),
            json!(bond_slaves_by_name(state, ifname)),
        );
    }
    let params = collect_params(ifcfg, &["bond_", "bond-"], BOND_V1_TO_V2);
    insert_parameters(entry, params);
    if let Some(keep) = ifcfg.get("keep_configuration") {
        entry.insert("critical".to_owned(), keep.clone());
    }
}

/// Returns false when the device has no ports and must be skipped entirely.
fn render_bridge(
    ifcfg: &Map<String, Value>,
    entry: &mut Map<String, Value>,
    warnings: &mut Warnings,
) -> bool {
    let Some(ports) = ifcfg.get("bridge_ports").and_then(Value::as_array) else {
        // Upstream's message is missing a space; keep it.
        warnings.0.push(format!(
            "Invalid config. The key'bridge_ports' is required in {}.",
            ci_core::jsonfmt::json_dumps(&Value::Object(ifcfg.clone()))
        ));
        return false;
    };
    let mut names: Vec<String> = ports
        .iter()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    names.sort();
    entry.insert("interfaces".to_owned(), json!(names));
    if let Some(mac) = ifcfg.get("mac_address") {
        entry.insert("macaddress".to_owned(), mac.clone());
    }

    let mut params = collect_params(ifcfg, &["bridge_"], BRIDGE_V1_TO_V2);
    // `path-cost`/`port-priority` take "<port> <value>" in v1 and a mapping
    // keyed by port in v2.
    for (key, value) in &mut params {
        if key != "path-cost" && key != "port-priority" {
            continue;
        }
        let mut mapping = Map::new();
        for item in listify(value) {
            let Some((port, val)) = item.split_once(' ') else {
                continue;
            };
            let Ok(number) = val.trim().parse::<i64>() else {
                continue;
            };
            mapping.insert(port.to_owned(), json!(number));
        }
        *value = Value::Object(mapping);
    }
    insert_parameters(entry, params);
    if let Some(keep) = ifcfg.get("keep_configuration") {
        entry.insert("critical".to_owned(), keep.clone());
    }
    true
}

fn render_vlan(ifcfg: &Map<String, Value>, entry: &mut Map<String, Value>) {
    entry.insert(
        "id".to_owned(),
        ifcfg.get("vlan_id").cloned().unwrap_or(Value::Null),
    );
    entry.insert(
        "link".to_owned(),
        ifcfg.get("vlan-raw-device").cloned().unwrap_or(Value::Null),
    );
    if let Some(mac) = ifcfg.get("mac_address") {
        entry.insert("macaddress".to_owned(), mac.clone());
    }
    if let Some(keep) = ifcfg.get("keep_configuration") {
        entry.insert("critical".to_owned(), keep.clone());
    }
}

fn insert_parameters(entry: &mut Map<String, Value>, mut params: Vec<(String, Value)>) {
    if params.is_empty() {
        return;
    }
    params.sort_by(|a, b| a.0.cmp(&b.0));
    entry.insert(
        "parameters".to_owned(),
        Value::Object(params.into_iter().collect()),
    );
}

/// `_extract_bond_slaves_by_name`.
fn bond_slaves_by_name(state: &NetworkState, master: &str) -> Vec<String> {
    let mut names: Vec<String> = state
        .interface_map()
        .values()
        .filter(|iface| {
            iface.get("bond-master").and_then(Value::as_str) == Some(master)
        })
        .filter_map(|iface| iface.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    names.sort();
    names
}

/// The `bond_`/`bond-`/`bridge_` prefixed keys, renamed to their v2 spellings.
fn collect_params(
    ifcfg: &Map<String, Value>,
    prefixes: &[&str],
    table: &[(&str, Option<&str>)],
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (key, value) in ifcfg {
        if !prefixes.iter().any(|p| key.starts_with(p)) {
            continue;
        }
        // v1 permits both separators; the table is keyed on the dashed form
        // for bonds and the underscored one for bridges.
        let lookup = if table.first().is_some_and(|(k, _)| k.starts_with("bond")) {
            key.replace('_', "-")
        } else {
            key.clone()
        };
        let Some((_, Some(v2_key))) = table.iter().find(|(v1, _)| *v1 == lookup) else {
            continue;
        };
        out.push(((*v2_key).to_owned(), value.clone()));
    }
    out
}

/// `_listify`: a space-separated string becomes a list, a scalar a singleton.
fn listify(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => s.split(' ').map(ToOwned::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => ci_core::jsonfmt::json_dumps(other),
            })
            .collect(),
        Value::Null => Vec::new(),
        other => vec![ci_core::jsonfmt::json_dumps(other)],
    }
}

/// `subnet_is_ipv6`.
#[must_use]
pub fn subnet_is_ipv6(subnet: &Value) -> bool {
    let kind = subnet.get("type").and_then(Value::as_str).unwrap_or("");
    if kind.ends_with('6') || IPV6_DYNAMIC_TYPES.contains(&kind) {
        return true;
    }
    kind == "static"
        && subnet
            .get("address")
            .and_then(Value::as_str)
            .is_some_and(ip::is_ipv6_address)
}

/// `should_add_gateway_onlink_flag`: a gateway outside its own subnet needs
/// `on-link`, and an unparseable pair is treated as "no flag" plus a warning.
fn gateway_needs_onlink(gateway: &str, subnet: &str, warnings: &mut Warnings) -> bool {
    ip::is_ip_in_subnet(gateway, subnet).map_or_else(
        || {
            warnings.0.push(format!(
                "Failed to check whether gateway {gateway} is contained \
                 within subnet {subnet}"
            ));
            false
        },
        |contained| !contained,
    )
}

/// `_extract_addresses`.
#[allow(clippy::too_many_lines)]
fn extract_addresses(
    ifcfg: &Map<String, Value>,
    entry: &mut Map<String, Value>,
    ifname: &str,
    features: Features,
    warnings: &mut Warnings,
) {
    let mut addresses: Vec<Value> = Vec::new();
    let mut routes: Vec<Value> = Vec::new();
    let mut nameservers: Vec<Value> = Vec::new();
    let mut searchdomains: Vec<Value> = Vec::new();
    let mut ipv4_mtu: Option<Value> = None;

    let subnets = ifcfg
        .get("subnets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for subnet in &subnets {
        let kind = subnet.get("type").and_then(Value::as_str).unwrap_or("");
        let metric = subnet.get("metric").filter(|v| !v.is_null()).cloned();

        if kind.starts_with("dhcp") {
            let key = if kind == "dhcp" { "dhcp4" } else { kind };
            entry.insert(key.to_owned(), Value::Bool(true));
            if let Some(metric) = metric.clone() {
                entry.insert(
                    format!("{key}-overrides"),
                    json!({"route-metric": metric}),
                );
            }
        } else if IPV6_DYNAMIC_TYPES.contains(&kind) {
            entry.insert("dhcp6".to_owned(), Value::Bool(true));
            if let Some(metric) = metric.clone() {
                entry.insert(
                    "dhcp6-overrides".to_owned(),
                    json!({"route-metric": metric}),
                );
            }
        } else if kind == "static" || kind == "static6" {
            let address = subnet.get("address").and_then(Value::as_str).unwrap_or("");
            let addr = match subnet.get("prefix").and_then(Value::as_u64) {
                Some(prefix) if !address.contains('/') => {
                    format!("{address}/{prefix}")
                }
                _ => address.to_owned(),
            };
            addresses.push(json!(addr));

            if let Some(gateway) = subnet.get("gateway").and_then(Value::as_str) {
                let mut route = Map::new();
                route.insert("via".to_owned(), json!(gateway));
                route.insert("to".to_owned(), json!("default"));
                if gateway_needs_onlink(gateway, &addr, warnings) {
                    route.insert("on-link".to_owned(), Value::Bool(true));
                }
                if let Some(metric) = metric.clone() {
                    route.insert("metric".to_owned(), metric);
                }
                routes.push(Value::Object(route));
            }
        }

        if let Some(dns) = subnet.get("dns_nameservers") {
            nameservers.extend(listify(dns).into_iter().map(|s| json!(s)));
        }
        if let Some(search) = subnet.get("dns_search") {
            searchdomains.extend(listify(search).into_iter().map(|s| json!(s)));
        }

        if let Some(mtu) = subnet.get("mtu").filter(|v| !v.is_null()) {
            if subnet_is_ipv6(subnet) && features.ipv6_mtu {
                entry.insert("ipv6-mtu".to_owned(), mtu.clone());
            } else {
                ipv4_mtu = Some(mtu.clone());
                entry.insert("mtu".to_owned(), mtu.clone());
            }
        }

        for route in subnet
            .get("routes")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let Some(network) = route.get("network").and_then(Value::as_str) else {
                continue;
            };
            let prefix = route.get("prefix").and_then(Value::as_u64);
            let to = match prefix {
                Some(prefix) if !network.contains('/') => {
                    format!("{network}/{prefix}")
                }
                _ => network.to_owned(),
            };
            let mut out = Map::new();
            if let Some(gateway) = route.get("gateway").filter(|v| !v.is_null()) {
                out.insert("via".to_owned(), gateway.clone());
            }
            out.insert("to".to_owned(), json!(to));
            let route_metric = route.get("metric").filter(|v| !v.is_null()).cloned();
            if let Some(metric) = route_metric.or(metric.clone()) {
                out.insert("metric".to_owned(), metric);
            }
            routes.push(Value::Object(out));
        }
    }

    if let Some(mtu) = ifcfg.get("mtu").filter(|v| !v.is_null()) {
        match &ipv4_mtu {
            Some(existing) if existing != mtu => {
                warnings.0.push(format!(
                    "Network config: ignoring {ifname} device-level mtu:{} \
                     because ipv4 subnet-level mtu:{} provided.",
                    ci_core::jsonfmt::json_dumps(mtu),
                    ci_core::jsonfmt::json_dumps(existing),
                ));
            }
            Some(_) => {}
            None => {
                entry.insert("mtu".to_owned(), mtu.clone());
            }
        }
    }

    if !addresses.is_empty() {
        entry.insert("addresses".to_owned(), Value::Array(addresses));
    }
    if !routes.is_empty() {
        entry.insert("routes".to_owned(), Value::Array(routes));
    }
    if !nameservers.is_empty() {
        entry
            .entry("nameservers".to_owned())
            .or_insert_with(|| json!({}))["addresses"] = Value::Array(nameservers);
    }
    if !searchdomains.is_empty() {
        entry
            .entry("nameservers".to_owned())
            .or_insert_with(|| json!({}))["search"] = Value::Array(searchdomains);
    }
    if let Some(accept_ra) = ifcfg.get("accept-ra").filter(|v| !v.is_null()) {
        entry.insert(
            "accept-ra".to_owned(),
            Value::Bool(matches!(accept_ra, Value::Bool(true))),
        );
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
    use crate::state::{parse_net_config_data, Target};

    fn render(config: &Value, target: Target) -> String {
        let mut parse_warnings = crate::state::Warnings::default();
        let state = parse_net_config_data(config, target, &mut parse_warnings).unwrap();
        let mut warnings = Warnings::default();
        render_content(&state, Features::default(), &mut warnings)
    }

    #[test]
    fn a_v1_static_address_renders_a_default_route() {
        let out = render(
            &json!({
                "version": 1,
                "config": [{
                    "type": "physical",
                    "name": "eth0",
                    "mac_address": "AA:BB:CC:DD:EE:FF",
                    "subnets": [{
                        "type": "static",
                        "address": "192.168.1.2/24",
                        "gateway": "192.168.1.1",
                    }],
                }],
            }),
            Target::Other,
        );
        assert_eq!(
            out,
            "network:\n    version: 2\n    ethernets:\n        eth0:\n\
             \x20           addresses:\n            - 192.168.1.2/24\n\
             \x20           match:\n                macaddress: aa:bb:cc:dd:ee:ff\n\
             \x20           routes:\n            -   to: default\n\
             \x20               via: 192.168.1.1\n            set-name: eth0\n"
        );
    }

    #[test]
    fn an_off_subnet_gateway_gets_the_onlink_flag() {
        let out = render(
            &json!({
                "version": 1,
                "config": [{
                    "type": "physical",
                    "name": "eth0",
                    "subnets": [{
                        "type": "static",
                        "address": "192.168.1.2/24",
                        "gateway": "10.0.0.1",
                    }],
                }],
            }),
            Target::Other,
        );
        assert!(out.contains("on-link: true"), "{out}");
    }

    #[test]
    fn netplan_v2_passes_the_config_straight_through() {
        let out = render(
            &json!({
                "version": 2,
                "ethernets": {"eth0": {"dhcp4": true}},
            }),
            Target::Netplan,
        );
        assert_eq!(
            out,
            "network:\n    ethernets:\n        eth0:\n            dhcp4: true\n\
             \x20   version: 2\n"
        );
    }

    #[test]
    fn dhcp_route_metrics_become_overrides() {
        let out = render(
            &json!({
                "version": 1,
                "config": [{
                    "type": "physical",
                    "name": "eth0",
                    "subnets": [{"type": "dhcp4", "metric": 100}],
                }],
            }),
            Target::Other,
        );
        assert!(out.contains("dhcp4: true"), "{out}");
        assert!(out.contains("route-metric: 100"), "{out}");
    }

    #[test]
    fn an_empty_header_still_opens_the_file_with_a_blank_line() {
        assert_eq!(render_with_header("network:\n", ""), "\nnetwork:\n");
        assert_eq!(
            render_with_header("network:\n", "# hi\n"),
            "# hi\nnetwork:\n"
        );
        assert_eq!(render_with_header("network:\n", "# hi"), "# hi\nnetwork:\n");
    }
}
