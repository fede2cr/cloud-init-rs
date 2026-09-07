//! `cloudinit.net.network_state`: both config formats, one internal model.
//!
//! Upstream's model is a nested dict built by mutating handlers, and the
//! renderers read it back by key. Keeping that shape — `serde_json::Value`
//! throughout, rather than a typed struct — is deliberate: the handlers copy
//! unknown keys straight through (`params`, `bond-*`, `bridge_*`), the
//! renderers select on prefixes rather than fields, and a typed model would
//! have to invent decisions for every key upstream simply carries. The v1
//! parser also *is* the v2 parser: v2 is lowered to v1 commands first.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::ip;

/// `NET_CONFIG_TO_V2["bond"]`, v1 key to v2 key.
pub const BOND_V1_TO_V2: &[(&str, Option<&str>)] = &[
    ("bond-ad-select", Some("ad-select")),
    ("bond-all-slaves-active", Some("all-slaves-active")),
    ("bond-arp-all-targets", Some("arp-all-targets")),
    ("bond-arp-interval", Some("arp-interval")),
    ("bond-arp-ip-target", Some("arp-ip-target")),
    ("bond-arp-validate", Some("arp-validate")),
    ("bond-downdelay", Some("down-delay")),
    ("bond-fail-over-mac", Some("fail-over-mac-policy")),
    ("bond-lacp-rate", Some("lacp-rate")),
    ("bond-learn-packet-interval", Some("learn-packet-interval")),
    ("bond-miimon", Some("mii-monitor-interval")),
    ("bond-min-links", Some("min-links")),
    ("bond-mode", Some("mode")),
    ("bond-num-grat-arp", Some("gratuitous-arp")),
    ("bond-packets-per-slave", Some("packets-per-slave")),
    ("bond-primary", Some("primary")),
    ("bond-primary-reselect", Some("primary-reselect-policy")),
    ("bond-updelay", Some("up-delay")),
    ("bond-xmit-hash-policy", Some("transmit-hash-policy")),
];

/// `NET_CONFIG_TO_V2["bridge"]`. The `None`s are v1 keys with no v2 spelling,
/// and they are dropped rather than passed through.
pub const BRIDGE_V1_TO_V2: &[(&str, Option<&str>)] = &[
    ("bridge_ageing", Some("ageing-time")),
    ("bridge_bridgeprio", Some("priority")),
    ("bridge_fd", Some("forward-delay")),
    ("bridge_gcint", None),
    ("bridge_hello", Some("hello-time")),
    ("bridge_maxage", Some("max-age")),
    ("bridge_maxwait", None),
    ("bridge_pathcost", Some("path-cost")),
    ("bridge_portprio", Some("port-priority")),
    ("bridge_stp", Some("stp")),
    ("bridge_waitport", None),
];

/// `IPV6_DYNAMIC_TYPES`.
pub const IPV6_DYNAMIC_TYPES: &[&str] = &[
    "dhcp6",
    "ipv6_slaac",
    "ipv6_dhcpv6-stateless",
    "ipv6_dhcpv6-stateful",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `RuntimeError("No handler found for command ...")`.
    NoHandler(String),
    /// `ValueError`/`TypeError` out of the normalizers.
    Invalid(String),
    /// The shape `parse_net_config_data` refuses outright.
    NoState,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHandler(cmd) => {
                write!(f, "No handler found for command '{cmd}'")
            }
            Self::Invalid(msg) => write!(f, "{msg}"),
            Self::NoState => write!(
                f,
                "No valid network_state object created from network config. \
                 Did you specify the correct version?"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// `InvalidCommand`, which `skip_broken` swallows.
#[derive(Debug)]
struct InvalidCommand(String);

/// What a single handler can go wrong with. Only `Skippable` is caught by
/// `skip_broken`; a `ValueError` out of the normalizers takes the parse down,
/// which is why a malformed address is a non-zero exit and not a warning.
#[derive(Debug)]
enum StepError {
    Skippable(String),
    Fatal(Error),
}

impl From<InvalidCommand> for StepError {
    fn from(e: InvalidCommand) -> Self {
        Self::Skippable(e.0)
    }
}

impl From<Error> for StepError {
    fn from(e: Error) -> Self {
        Self::Fatal(e)
    }
}

/// The parsed result the renderers consume.
///
/// Upstream keeps this as one nested dict; the top level is broken out into
/// fields here because those five keys are fixed and every access to them would
/// otherwise be an unchecked index into a `Value`.
#[derive(Debug, Clone, Default)]
pub struct NetworkState {
    version: u64,
    /// The raw input config, kept for the v2 netplan passthrough.
    config: Value,
    interfaces: Map<String, Value>,
    routes: Vec<Value>,
    dns_nameservers: Vec<Value>,
    dns_search: Vec<Value>,
    use_ipv6: bool,
    /// `to_passthrough` copies only `config`; nothing else is interpreted.
    passthrough: bool,
}

impl NetworkState {
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    /// `NetworkState.config`.
    #[must_use]
    pub fn config(&self) -> &Value {
        &self.config
    }

    /// `NetworkState.dns_nameservers`.
    #[must_use]
    pub fn dns_nameservers(&self) -> &[Value] {
        &self.dns_nameservers
    }

    /// `NetworkState.dns_searchdomains`.
    #[must_use]
    pub fn dns_searchdomains(&self) -> &[Value] {
        &self.dns_search
    }

    /// `iter_interfaces`, in insertion order as upstream's dict yields it.
    #[must_use]
    pub fn interfaces(&self) -> Vec<&Value> {
        self.interfaces.values().collect()
    }

    /// The interface map itself, which the bond renderer walks by slave name.
    #[must_use]
    pub fn interface_map(&self) -> &Map<String, Value> {
        &self.interfaces
    }

    /// `iter_routes`.
    #[must_use]
    pub fn routes(&self) -> &[Value] {
        &self.routes
    }

    /// `NetworkState.use_ipv6`.
    #[must_use]
    pub fn use_ipv6(&self) -> bool {
        self.use_ipv6
    }

    /// `has_default_route`.
    #[must_use]
    pub fn has_default_route(&self) -> bool {
        let is_default = |route: &Value| {
            route.get("prefix").and_then(Value::as_u64) == Some(0)
                && matches!(
                    route.get("network").and_then(Value::as_str),
                    Some("::" | "0.0.0.0")
                )
        };
        if self.routes.iter().any(is_default) {
            return true;
        }
        self.interfaces.values().any(|iface| {
            iface
                .get("subnets")
                .and_then(Value::as_array)
                .is_some_and(|subnets| {
                    subnets.iter().any(|s| {
                        s.get("routes")
                            .and_then(Value::as_array)
                            .is_some_and(|r| r.iter().any(is_default))
                    })
                })
        })
    }
}

/// Whether the renderer being targeted is netplan, which changes v2 handling:
/// netplan gets the config back untouched instead of a round trip through v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Netplan,
    Other,
}

/// Warnings the parse produced, so a caller can log them the way upstream does.
#[derive(Debug, Default, Clone)]
pub struct Warnings(pub Vec<String>);

/// `parse_net_config_data`.
pub fn parse_net_config_data(
    net_config: &Value,
    target: Target,
    warnings: &mut Warnings,
) -> Result<NetworkState, Error> {
    let version = net_config.get("version").and_then(Value::as_u64);
    let config = match version {
        // v2 has no explicit `config` key; the whole document is the config.
        Some(2) => Some(net_config.clone()),
        _ => net_config.get("config").cloned(),
    };
    let (Some(version), Some(config)) = (version, config) else {
        return Err(Error::NoState);
    };
    if config.is_null() {
        return Err(Error::NoState);
    }

    if version == 2 && target == Target::Netplan {
        return Ok(NetworkState {
            version: 2,
            config,
            passthrough: true,
            ..NetworkState::default()
        });
    }

    let mut interp = Interpreter::new(config.clone(), warnings);
    match version {
        1 => interp.parse_v1()?,
        2 => interp.parse_v2()?,
        // Upstream leaves `state` as None and then raises the same error.
        _ => return Err(Error::NoState),
    }

    Ok(NetworkState {
        version,
        config,
        interfaces: interp.interfaces,
        routes: interp.routes,
        dns_nameservers: interp.dns_nameservers,
        dns_search: interp.dns_search,
        use_ipv6: interp.use_ipv6,
        passthrough: false,
    })
}

struct Interpreter<'a> {
    config: Value,
    interfaces: Map<String, Value>,
    routes: Vec<Value>,
    dns_nameservers: Vec<Value>,
    dns_search: Vec<Value>,
    use_ipv6: bool,
    /// v1 `nameserver` commands naming an interface, applied after the walk.
    interface_dns: BTreeMap<String, (Vec<Value>, Vec<Value>)>,
    warnings: &'a mut Warnings,
}

impl<'a> Interpreter<'a> {
    fn new(config: Value, warnings: &'a mut Warnings) -> Self {
        Self {
            config,
            interfaces: Map::new(),
            routes: Vec::new(),
            dns_nameservers: Vec::new(),
            dns_search: Vec::new(),
            use_ipv6: false,
            interface_dns: BTreeMap::new(),
            warnings,
        }
    }

    fn parse_v1(&mut self) -> Result<(), Error> {
        let commands = self
            .config
            .as_array()
            .cloned()
            .ok_or_else(|| Error::NoHandler(String::new()))?;
        for command in &commands {
            let kind = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !is_v1_command(&kind) {
                // Upstream's message doubles the space; it is user-visible.
                return Err(Error::NoHandler(format!(" {kind}")));
            }
            match self.dispatch_v1(&kind, command) {
                Ok(()) => {}
                Err(StepError::Skippable(msg)) => self
                    .warnings
                    .0
                    .push(format!("Skipping invalid command: {msg}")),
                Err(StepError::Fatal(e)) => return Err(e),
            }
        }

        let dns = std::mem::take(&mut self.interface_dns);
        for (name, (nameservers, search)) in dns {
            let iface = self.interfaces.get_mut(&name).ok_or_else(|| {
                Error::Invalid(format!(
                    "Nameserver specified for interface {name}, but \
                         interface {name} does not exist!"
                ))
            })?;
            set(
                iface,
                "dns",
                json!({"nameservers": nameservers, "search": search}),
            );
        }
        Ok(())
    }

    fn dispatch_v1(&mut self, kind: &str, command: &Value) -> Result<(), StepError> {
        match kind {
            "physical" | "loopback" | "infiniband" => {
                require(command, &["name"])?;
                self.handle_physical(command)?;
                Ok(())
            }
            "vlan" => {
                require(command, &["name", "vlan_id", "vlan_link"])?;
                self.handle_vlan(command)?;
                Ok(())
            }
            "bond" => {
                require(command, &["name", "bond_interfaces", "params"])?;
                self.handle_bond(command)?;
                Ok(())
            }
            "bridge" => {
                require(command, &["name", "bridge_interfaces"])?;
                self.handle_bridge(command)
            }
            "nameserver" => {
                require(command, &["address"])?;
                self.handle_nameserver(command);
                Ok(())
            }
            "route" => {
                require(command, &["destination"])?;
                self.handle_route(command)
            }
            _ => Ok(()),
        }
    }

    fn parse_v2(&mut self) -> Result<(), Error> {
        let config = self
            .config
            .as_object()
            .cloned()
            .ok_or_else(|| Error::NoHandler(String::new()))?;
        for (kind, command) in &config {
            if kind == "version" || kind == "renderer" {
                continue;
            }
            if !is_v2_command(kind) {
                return Err(Error::NoHandler(kind.clone()));
            }
            let result = match kind.as_str() {
                "bonds" => self.handle_bond_bridge(command, "bond"),
                "bridges" => self.handle_bond_bridge(command, "bridge"),
                "ethernets" => self.handle_ethernets(command),
                "vlans" => self.handle_vlans(command),
                "wifis" => {
                    self.warnings.0.push(
                        "Wifi configuration is only available to distros with \
                         netplan rendering support."
                            .to_owned(),
                    );
                    Ok(())
                }
                _ => Ok(()),
            };
            match result {
                Ok(()) => self.v2_common(command),
                Err(StepError::Skippable(msg)) => self
                    .warnings
                    .0
                    .push(format!("Skipping invalid command: {msg}")),
                Err(StepError::Fatal(e)) => return Err(e),
            }
        }
        Ok(())
    }

    fn interfaces_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.interfaces
    }

    fn set_use_ipv6(&mut self) {
        self.use_ipv6 = true;
    }

    /// `handle_physical`, which every other interface handler runs first.
    fn handle_physical(&mut self, command: &Value) -> Result<(), Error> {
        let name = command.get("name").cloned().unwrap_or(Value::Null);
        let key = command
            .get("config_id")
            .filter(|v| !v.is_null())
            .or_else(|| command.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let mut iface = self
            .interfaces_mut()
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        if let Some(params) = command.get("params").and_then(Value::as_object) {
            for (param, val) in params {
                iface.insert(param.clone(), val.clone());
            }
        }

        let subnets = normalize_subnets(command.get("subnets"))?;
        if !self.use_ipv6 {
            let ipv6 = subnets.iter().any(|s| {
                s.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.ends_with('6'))
                    || s.get("address")
                        .and_then(Value::as_str)
                        .is_some_and(ip::is_ipv6_address)
            });
            if ipv6 {
                self.set_use_ipv6();
            }
        }

        let tristate = |key: &str| {
            command
                .get(key)
                .filter(|v| !v.is_null())
                .map_or(Value::Null, |v| Value::Bool(is_true(v)))
        };

        let mac = command
            .get("mac_address")
            .and_then(Value::as_str)
            .map_or(Value::Null, |m| Value::String(m.to_lowercase()));

        iface.insert(
            "config_id".to_owned(),
            command.get("config_id").cloned().unwrap_or(Value::Null),
        );
        iface.insert("name".to_owned(), name);
        iface.insert(
            "type".to_owned(),
            command.get("type").cloned().unwrap_or(Value::Null),
        );
        iface.insert("mac_address".to_owned(), mac);
        iface.insert("inet".to_owned(), json!("inet"));
        iface.insert("mode".to_owned(), json!("manual"));
        iface.insert(
            "mtu".to_owned(),
            command.get("mtu").cloned().unwrap_or(Value::Null),
        );
        iface.insert("address".to_owned(), Value::Null);
        iface.insert("gateway".to_owned(), Value::Null);
        iface.insert("subnets".to_owned(), Value::Array(subnets));
        iface.insert("accept-ra".to_owned(), tristate("accept-ra"));
        iface.insert("wakeonlan".to_owned(), tristate("wakeonlan"));
        iface.insert("optional".to_owned(), tristate("optional"));
        iface.insert(
            "keep_configuration".to_owned(),
            command
                .get("keep_configuration")
                .cloned()
                .unwrap_or(Value::Null),
        );

        self.interfaces_mut().insert(key, Value::Object(iface));
        Ok(())
    }

    fn handle_vlan(&mut self, command: &Value) -> Result<(), Error> {
        self.handle_physical(command)?;
        let name = command
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let link = command.get("vlan_link").cloned().unwrap_or(Value::Null);
        let id = command.get("vlan_id").cloned().unwrap_or(Value::Null);
        if let Some(iface) = self.interfaces_mut().get_mut(&name) {
            set(iface, "vlan-raw-device", link);
            set(iface, "vlan_id", id);
        }
        Ok(())
    }

    fn handle_bond(&mut self, command: &Value) -> Result<(), Error> {
        self.handle_physical(command)?;
        let name = command
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = command
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        if let Some(iface) = self.interfaces_mut().get_mut(&name) {
            for (param, val) in &params {
                set(iface, param, val.clone());
            }
            set(iface, "bond-slaves", json!("none"));
        }

        let slaves: Vec<String> = command
            .get("bond_interfaces")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        for slave in slaves {
            if !self.interfaces_mut().contains_key(&slave) {
                // The placeholder upstream injects, type "bond" and all.
                self.handle_physical(&json!({"name": slave, "type": "bond"}))?;
            }
            if let Some(bond_if) = self.interfaces_mut().get_mut(&slave) {
                set(bond_if, "bond-master", Value::String(name.clone()));
                for (param, val) in &params {
                    set(bond_if, param, val.clone());
                }
            }
        }
        Ok(())
    }

    fn handle_bridge(&mut self, command: &Value) -> Result<(), StepError> {
        let ports: Vec<String> = command
            .get("bridge_interfaces")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        for port in &ports {
            if !self.interfaces_mut().contains_key(port) {
                // Note the placeholder has no `type`, unlike the bond one.
                self.handle_physical(&json!({"name": port}))?;
            }
        }

        self.handle_physical(command)?;
        let name = command
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let bridge_ports = command
            .get("bridge_interfaces")
            .cloned()
            .unwrap_or(Value::Null);
        let params = command
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let Some(iface) = self.interfaces_mut().get_mut(&name) else {
            return Ok(());
        };
        set(iface, "bridge_ports", bridge_ports);
        for (param, val) in &params {
            set(iface, param, val.clone());
        }

        let normalized = match iface.get("bridge_stp") {
            None | Some(Value::Null | Value::Bool(_)) => None,
            Some(stp) => {
                let value = match stp {
                    Value::String(s) if s == "on" || s == "1" => Some(true),
                    Value::String(s) if s == "off" || s == "0" => Some(false),
                    Value::Number(n) if n.as_i64() == Some(1) => Some(true),
                    Value::Number(n) if n.as_i64() == Some(0) => Some(false),
                    _ => None,
                };
                let Some(value) = value else {
                    let text = scalar_str(stp);
                    return Err(StepError::Skippable(format!(
                        "Cannot convert bridge_stp value ({text}) to boolean"
                    )));
                };
                Some(value)
            }
        };
        if let Some(value) = normalized {
            set(iface, "bridge_stp", Value::Bool(value));
        }
        Ok(())
    }

    fn handle_nameserver(&mut self, command: &Value) {
        let (nameservers, search) = parse_dns(command);
        if let Some(interface) = command.get("interface").and_then(Value::as_str) {
            self.interface_dns
                .insert(interface.to_owned(), (nameservers, search));
            return;
        }
        self.dns_nameservers.extend(nameservers);
        self.dns_search.extend(search);
    }

    fn handle_route(&mut self, command: &Value) -> Result<(), StepError> {
        let route = normalize_route(command)?;
        self.routes.push(route);
        Ok(())
    }

    // --- v2 lowering ------------------------------------------------------

    fn handle_ethernets(&mut self, command: &Value) -> Result<(), StepError> {
        let Some(entries) = command.as_object() else {
            return Ok(());
        };
        for (eth, cfg) in entries {
            let mut phy = Map::new();
            phy.insert("config_id".to_owned(), Value::String(eth.clone()));
            phy.insert("type".to_owned(), json!("physical"));

            let mac = cfg
                .get("match")
                .and_then(|m| m.get("macaddress"))
                .cloned()
                .unwrap_or(Value::Null);
            phy.insert("mac_address".to_owned(), mac);

            // set-name wins; the mac lookup upstream does next needs a live
            // system, and net-convert's `--mac` is the only way to supply one.
            let name = cfg
                .get("set-name")
                .and_then(Value::as_str)
                .unwrap_or(eth)
                .to_owned();
            phy.insert("name".to_owned(), Value::String(name));

            if let Some(driver) =
                cfg.get("match").and_then(|m| m.get("driver")).cloned()
            {
                if !driver.is_null() {
                    phy.insert("params".to_owned(), json!({"driver": driver}));
                }
            }
            for key in ["mtu", "match", "wakeonlan", "accept-ra", "optional"] {
                if let Some(v) = cfg.get(key) {
                    phy.insert(key.to_owned(), v.clone());
                }
            }
            self.warn_deprecated_gateways(cfg);

            let subnets = self.v2_to_v1_ipcfg(cfg)?;
            if !subnets.is_empty() {
                phy.insert("subnets".to_owned(), Value::Array(subnets));
            }
            self.handle_physical(&Value::Object(phy))?;
        }
        Ok(())
    }

    fn handle_vlans(&mut self, command: &Value) -> Result<(), StepError> {
        let Some(entries) = command.as_object() else {
            return Ok(());
        };
        for (vlan, cfg) in entries {
            let mut cmd = Map::new();
            cmd.insert("type".to_owned(), json!("vlan"));
            cmd.insert("name".to_owned(), Value::String(vlan.clone()));
            cmd.insert(
                "vlan_id".to_owned(),
                cfg.get("id").cloned().unwrap_or(Value::Null),
            );
            cmd.insert(
                "vlan_link".to_owned(),
                cfg.get("link").cloned().unwrap_or(Value::Null),
            );
            cmd.insert(
                "mac_address".to_owned(),
                cfg.get("macaddress").cloned().unwrap_or(Value::Null),
            );
            if let Some(mtu) = cfg.get("mtu") {
                cmd.insert("mtu".to_owned(), mtu.clone());
            }
            self.warn_deprecated_gateways(cfg);
            let subnets = self.v2_to_v1_ipcfg(cfg)?;
            if !subnets.is_empty() {
                cmd.insert("subnets".to_owned(), Value::Array(subnets));
            }
            self.handle_vlan(&Value::Object(cmd))?;
        }
        Ok(())
    }

    fn handle_bond_bridge(
        &mut self,
        command: &Value,
        kind: &str,
    ) -> Result<(), StepError> {
        let Some(entries) = command.as_object() else {
            return Ok(());
        };
        let table = if kind == "bond" {
            BOND_V1_TO_V2
        } else {
            BRIDGE_V1_TO_V2
        };

        for (item_name, item_cfg) in entries {
            let mut params = item_cfg
                .get("parameters")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // netplan accepts both spellings; normalize to the correct one.
            if let Some(grat) = params.remove("gratuitious-arp") {
                if is_truthy(&grat) {
                    params.insert("gratuitous-arp".to_owned(), grat);
                }
            }

            let mut v1_params = Map::new();
            for (v2_key, value) in &params {
                let Some(v1_key) = table
                    .iter()
                    .find(|(_, v2)| *v2 == Some(v2_key.as_str()))
                    .map(|(v1, _)| *v1)
                else {
                    // Upstream raises KeyError here, which is not an
                    // InvalidCommand and takes the whole parse down.
                    return Err(StepError::Fatal(Error::Invalid(format!(
                        "Unknown {kind} parameter '{v2_key}'"
                    ))));
                };
                v1_params.insert(v1_key.to_owned(), value.clone());
            }

            let mut cmd = Map::new();
            cmd.insert("type".to_owned(), Value::String(kind.to_owned()));
            cmd.insert("name".to_owned(), Value::String(item_name.clone()));
            cmd.insert(
                format!("{kind}_interfaces"),
                item_cfg.get("interfaces").cloned().unwrap_or(Value::Null),
            );
            cmd.insert("params".to_owned(), Value::Object(v1_params));
            if let Some(mtu) = item_cfg.get("mtu") {
                cmd.insert("mtu".to_owned(), mtu.clone());
            }
            if let Some(mac) = item_cfg.get("macaddress") {
                cmd.insert("mac_address".to_owned(), mac.clone());
            }
            self.warn_deprecated_gateways(item_cfg);
            let subnets = self.v2_to_v1_ipcfg(item_cfg)?;
            if !subnets.is_empty() {
                cmd.insert("subnets".to_owned(), Value::Array(subnets));
            }

            let cmd = Value::Object(cmd);
            if kind == "bridge" {
                self.handle_bridge(&cmd)?;
            } else {
                self.handle_bond(&cmd)?;
            }
        }
        Ok(())
    }

    /// `_v2_common`: per-device `nameservers` become that device's `dns`.
    fn v2_common(&mut self, cfg: &Value) {
        let Some(entries) = cfg.as_object() else {
            return;
        };
        for (iface, dev_cfg) in entries {
            let Some(ns) = dev_cfg.get("nameservers") else {
                continue;
            };
            let search = ns.get("search").cloned().unwrap_or(Value::Null);
            let dns = ns.get("addresses").cloned().unwrap_or(Value::Null);
            let mut cmd = Map::new();
            cmd.insert("type".to_owned(), json!("nameserver"));
            if !search.is_null() {
                cmd.insert("search".to_owned(), search);
            }
            if !dns.is_null() {
                cmd.insert("address".to_owned(), dns);
            }
            let cmd = Value::Object(cmd);
            if cmd.get("address").is_none() {
                // `@ensure_command_keys(["address"])` rejects it, and the
                // caller does not catch InvalidCommand here.
                continue;
            }
            let (nameservers, search) = parse_dns(&cmd);
            if let Some(entry) = self.interfaces_mut().get_mut(iface) {
                entry["dns"] = json!({"nameservers": nameservers, "search": search});
            }
        }
    }

    fn warn_deprecated_gateways(&mut self, cfg: &Value) {
        if cfg.get("gateway4").is_some() || cfg.get("gateway6").is_some() {
            self.warnings.0.push(
                "The use of `gateway4` and `gateway6` is deprecated in 22.4 \
                 and scheduled to be removed in 27.4."
                    .to_owned(),
            );
        }
    }

    /// `_v2_to_v1_ipcfg`.
    fn v2_to_v1_ipcfg(&mut self, cfg: &Value) -> Result<Vec<Value>, StepError> {
        let mut subnets: Vec<Value> = Vec::new();

        let dhcp_metric = |key: &str, subnet: &mut Map<String, Value>| {
            if let Some(metric) = cfg
                .get(key)
                .and_then(|o| o.get("route-metric"))
                .filter(|v| !v.is_null())
            {
                subnet.insert("metric".to_owned(), metric.clone());
            }
        };

        if cfg.get("dhcp4").is_some_and(is_truthy) {
            let mut subnet = Map::new();
            subnet.insert("type".to_owned(), json!("dhcp4"));
            dhcp_metric("dhcp4-overrides", &mut subnet);
            subnets.push(Value::Object(subnet));
        }
        if cfg.get("dhcp6").is_some_and(is_truthy) {
            let mut subnet = Map::new();
            subnet.insert("type".to_owned(), json!("dhcp6"));
            self.set_use_ipv6();
            dhcp_metric("dhcp6-overrides", &mut subnet);
            subnets.push(Value::Object(subnet));
        }

        let mut gateway4_taken = false;
        let mut gateway6_taken = false;
        let mut nameservers_taken = false;
        for address in cfg
            .get("addresses")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let mut subnet = Map::new();
            subnet.insert("type".to_owned(), json!("static"));
            subnet.insert("address".to_owned(), address.clone());

            let is_v6 = address.as_str().is_some_and(|a| a.contains(':'));
            if is_v6 {
                if !gateway6_taken {
                    if let Some(gw) = cfg.get("gateway6") {
                        gateway6_taken = true;
                        subnet.insert("gateway".to_owned(), gw.clone());
                    }
                }
            } else if !gateway4_taken {
                if let Some(gw) = cfg.get("gateway4") {
                    gateway4_taken = true;
                    subnet.insert("gateway".to_owned(), gw.clone());
                }
            }

            if !nameservers_taken {
                if let Some(ns) = cfg.get("nameservers") {
                    let addresses = ns.get("addresses").filter(|v| !v.is_null());
                    let search = ns.get("search").filter(|v| !v.is_null());
                    if let Some(addresses) = addresses {
                        subnet.insert("dns_nameservers".to_owned(), addresses.clone());
                        nameservers_taken = true;
                    }
                    if let Some(search) = search {
                        subnet.insert("dns_search".to_owned(), search.clone());
                        nameservers_taken = true;
                    }
                }
            }
            subnets.push(Value::Object(subnet));
        }

        let mut routes: Vec<Value> = Vec::new();
        for route in cfg
            .get("routes")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let mut src = Map::new();
            for (from, to) in [
                ("to", "destination"),
                ("via", "gateway"),
                ("metric", "metric"),
                ("mtu", "mtu"),
                ("table", "table"),
            ] {
                src.insert(
                    to.to_owned(),
                    route.get(from).cloned().unwrap_or(Value::Null),
                );
            }
            routes.push(normalize_route(&Value::Object(src))?);
        }

        // v2 binds routes to the interface; v1 has no such level, so they go
        // under the first subnet.
        if let (Some(first), false) = (subnets.first_mut(), routes.is_empty()) {
            set(first, "routes", Value::Array(routes));
        }

        Ok(subnets)
    }
}

fn is_v1_command(kind: &str) -> bool {
    matches!(
        kind,
        "bond"
            | "bridge"
            | "infiniband"
            | "loopback"
            | "nameserver"
            | "physical"
            | "route"
            | "vlan"
    )
}

fn is_v2_command(kind: &str) -> bool {
    matches!(kind, "bonds" | "bridges" | "ethernets" | "vlans" | "wifis")
}

/// `@ensure_command_keys`.
fn require(command: &Value, keys: &[&str]) -> Result<(), InvalidCommand> {
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| command.get(*k).is_none())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(InvalidCommand(format!(
        "Command missing {missing:?} of required keys {keys:?}"
    )))
}

/// `util.is_true`.
fn is_true(value: &Value) -> bool {
    match value {
        Value::Bool(b) => *b,
        Value::String(s) => {
            matches!(
                s.trim().to_lowercase().as_str(),
                "true" | "1" | "on" | "yes"
            )
        }
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => false,
    }
}

/// Python truthiness, which is what the bare `if cfg.get(...)` checks use.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `value[key] = new`, for a `Value` that should be an object. Anything else
/// is left alone rather than panicking, since none of these keys are reachable
/// on a non-object in practice.
fn set(value: &mut Value, key: &str, new: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_owned(), new);
    }
}

/// `_parse_dns`: a scalar is promoted to a one-element list.
fn parse_dns(command: &Value) -> (Vec<Value>, Vec<Value>) {
    let listify = |key: &str| -> Vec<Value> {
        match command.get(key) {
            None => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(other) => vec![other.clone()],
        }
    };
    (listify("address"), listify("search"))
}

fn normalize_subnets(subnets: Option<&Value>) -> Result<Vec<Value>, Error> {
    let Some(items) = subnets.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    items.iter().map(normalize_subnet).collect()
}

/// `_normalize_subnet`.
fn normalize_subnet(subnet: &Value) -> Result<Value, Error> {
    let mut normal: Map<String, Value> = subnet
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| is_truthy(v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let kind = subnet.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "static" || kind == "static6" {
        let keys = normalize_net_keys(
            &Value::Object(normal.clone()),
            &["address", "ip_address"],
        )?;
        for (k, v) in keys {
            normal.insert(k, v);
        }
    }

    let routes: Vec<Value> = subnet
        .get("routes")
        .and_then(Value::as_array)
        .map(|rs| rs.iter().map(normalize_route).collect::<Result<_, _>>())
        .transpose()?
        .unwrap_or_default();
    normal.insert("routes".to_owned(), Value::Array(routes));

    // A whitespace-separated string is a list here.
    for key in ["dns_search", "dns_nameservers"] {
        if let Some(Value::String(s)) = normal.get(key).cloned() {
            let items: Vec<Value> = s.split_whitespace().map(|w| json!(w)).collect();
            normal.insert(key.to_owned(), Value::Array(items));
        }
    }

    Ok(Value::Object(normal))
}

/// `_normalize_net_keys`: resolves address/prefix/netmask into all three.
fn normalize_net_keys(
    network: &Value,
    address_keys: &[&str],
) -> Result<Map<String, Value>, Error> {
    let mut net: Map<String, Value> = network
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| {
                    is_truthy(v) || v.as_i64() == Some(0) || v.as_f64() == Some(0.0)
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let addr_key = address_keys
        .iter()
        .copied()
        .find(|k| net.get(*k).is_some_and(is_truthy))
        .ok_or_else(|| {
            Error::Invalid(format!(
                "No config network address keys [{}] found in {}",
                address_keys.join(","),
                ci_core::jsonfmt::json_dumps(network)
            ))
        })?;

    let mut addr = net.get(addr_key).map(scalar_str).unwrap_or_default();
    if addr == "default" {
        let gw_ip = net.get("gateway").map(scalar_str).unwrap_or_default();
        if gw_ip.is_empty() {
            return Err(Error::Invalid("Gateway IP is empty".to_owned()));
        }
        addr = if ip::is_ipv4_address(&gw_ip) {
            "0.0.0.0/0".to_owned()
        } else if ip::is_ipv6_address(&gw_ip) {
            "::/0".to_owned()
        } else {
            return Err(Error::Invalid(format!("Invalid Gateway IP: '{gw_ip}'")));
        };
    }

    if !ip::is_ip_network(&addr) {
        return Err(Error::Invalid(format!(
            "Address {addr} is not a valid ip address"
        )));
    }
    let ipv6 = ip::is_ipv6_network(&addr);
    let ipv4 = ip::is_ipv4_network(&addr);

    let netmask = net.get("netmask").map(scalar_str);
    let prefix: u8 = if let Some((addr_part, maybe_prefix)) = addr.split_once('/') {
        net.insert(addr_key.to_owned(), json!(addr_part));
        let converted = if ipv6 {
            ip::ipv6_mask_to_net_prefix(maybe_prefix)
        } else {
            ip::ipv4_mask_to_net_prefix(maybe_prefix)
        };
        converted.ok_or_else(|| {
            Error::Invalid(format!("Address {addr} is not a valid ip address"))
        })?
    } else if let Some(existing) = net.get("prefix") {
        parse_int(existing).ok_or_else(|| {
            Error::Invalid(format!(
                "invalid literal for int(): {}",
                scalar_str(existing)
            ))
        })?
    } else if let Some(netmask) = netmask.as_deref().filter(|m| !m.is_empty()) {
        let converted = if ipv4 {
            ip::ipv4_mask_to_net_prefix(netmask)
        } else if ipv6 {
            ip::ipv6_mask_to_net_prefix(netmask)
        } else {
            None
        };
        converted.ok_or_else(|| {
            Error::Invalid(format!("Invalid network mask '{netmask}'"))
        })?
    } else if ipv6 {
        64
    } else {
        24
    };

    net.insert("prefix".to_owned(), json!(prefix));
    if ipv6 {
        net.remove("netmask");
    } else if ipv4 {
        net.insert(
            "netmask".to_owned(),
            json!(ip::net_prefix_to_ipv4_mask(prefix)),
        );
    }
    Ok(net)
}

/// `_normalize_route`.
fn normalize_route(route: &Value) -> Result<Value, Error> {
    let mut normal: Map<String, Value> = route
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, v)| {
                    !v.is_null() && v.as_str().is_none_or(|s| !s.is_empty())
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    if let Some(destination) = normal.remove("destination") {
        normal.insert("network".to_owned(), destination);
    }

    let keys = normalize_net_keys(
        &Value::Object(normal.clone()),
        &["network", "destination"],
    )?;
    for (k, v) in keys {
        normal.insert(k, v);
    }

    if let Some(metric) = normal.get("metric").cloned() {
        if is_truthy(&metric) {
            let parsed = parse_int(&metric).ok_or_else(|| {
                Error::Invalid(format!(
                    "Route config metric {} is not an integer",
                    scalar_str(&metric)
                ))
            })?;
            normal.insert("metric".to_owned(), json!(parsed));
        }
    }
    Ok(Value::Object(normal))
}

/// `str(value)` for the scalars that reach these paths.
fn scalar_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => ci_core::jsonfmt::json_dumps(other),
    }
}

fn parse_int(value: &Value) -> Option<u8> {
    match value {
        Value::Number(n) => u8::try_from(n.as_u64()?).ok(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
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

    fn parse(config: &Value) -> NetworkState {
        let mut warnings = Warnings::default();
        parse_net_config_data(config, Target::Other, &mut warnings).unwrap()
    }

    #[test]
    fn v1_static_subnet_gains_prefix_and_netmask() {
        let state = parse(&json!({
            "version": 1,
            "config": [{
                "type": "physical",
                "name": "eth0",
                "subnets": [{"type": "static", "address": "192.168.1.2/24"}],
            }],
        }));
        let iface = state.interfaces()[0];
        let subnet = &iface["subnets"][0];
        assert_eq!(subnet["address"], json!("192.168.1.2"));
        assert_eq!(subnet["prefix"], json!(24));
        assert_eq!(subnet["netmask"], json!("255.255.255.0"));
    }

    #[test]
    fn v1_netmask_key_is_converted_to_a_prefix() {
        let state = parse(&json!({
            "version": 1,
            "config": [{
                "type": "physical",
                "name": "eth0",
                "subnets": [{
                    "type": "static",
                    "address": "10.0.0.2",
                    "netmask": "255.255.0.0",
                }],
            }],
        }));
        assert_eq!(state.interfaces()[0]["subnets"][0]["prefix"], json!(16));
    }

    #[test]
    fn ipv6_subnet_sets_use_ipv6_and_drops_netmask() {
        let state = parse(&json!({
            "version": 1,
            "config": [{
                "type": "physical",
                "name": "eth0",
                "subnets": [{"type": "static6", "address": "2001:db8::1/64"}],
            }],
        }));
        assert!(state.use_ipv6());
        let subnet = &state.interfaces()[0]["subnets"][0];
        assert_eq!(subnet["prefix"], json!(64));
        assert!(subnet.get("netmask").is_none());
    }

    #[test]
    fn a_default_route_destination_follows_the_gateway_family() {
        let state = parse(&json!({
            "version": 1,
            "config": [
                {"type": "physical", "name": "eth0"},
                {"type": "route", "destination": "default",
                 "gateway": "192.168.1.1"},
            ],
        }));
        let route = &state.routes()[0];
        assert_eq!(route["network"], json!("0.0.0.0"));
        assert_eq!(route["prefix"], json!(0));
        assert!(state.has_default_route());
    }

    #[test]
    fn v2_ethernet_lowers_to_a_v1_physical() {
        let state = parse(&json!({
            "version": 2,
            "ethernets": {
                "eth0": {
                    "match": {"macaddress": "AA:BB:CC:DD:EE:FF"},
                    "addresses": ["192.168.1.2/24"],
                    "gateway4": "192.168.1.1",
                },
            },
        }));
        let iface = state.interfaces()[0];
        assert_eq!(iface["type"], json!("physical"));
        assert_eq!(iface["mac_address"], json!("aa:bb:cc:dd:ee:ff"));
        assert_eq!(iface["subnets"][0]["gateway"], json!("192.168.1.1"));
    }

    #[test]
    fn only_the_first_address_of_a_family_takes_the_gateway() {
        let state = parse(&json!({
            "version": 2,
            "ethernets": {"eth0": {
                "addresses": ["192.168.1.2/24", "192.168.1.3/24"],
                "gateway4": "192.168.1.1",
            }},
        }));
        let subnets = state.interfaces()[0]["subnets"].as_array().unwrap();
        assert_eq!(subnets[0]["gateway"], json!("192.168.1.1"));
        assert!(subnets[1].get("gateway").is_none());
    }

    #[test]
    fn v2_bond_parameters_are_renamed_to_v1() {
        let state = parse(&json!({
            "version": 2,
            "bonds": {"bond0": {
                "interfaces": ["eth0", "eth1"],
                "parameters": {"mode": "802.3ad", "mii-monitor-interval": 100},
            }},
        }));
        let map = state.interface_map();
        assert_eq!(map["bond0"]["bond-mode"], json!("802.3ad"));
        assert_eq!(map["bond0"]["bond-slaves"], json!("none"));
        assert_eq!(map["eth0"]["bond-master"], json!("bond0"));
        assert_eq!(map["eth0"]["bond-mode"], json!("802.3ad"));
    }

    #[test]
    fn bridge_stp_strings_become_booleans() {
        let state = parse(&json!({
            "version": 1,
            "config": [{
                "type": "bridge",
                "name": "br0",
                "bridge_interfaces": ["eth0"],
                "params": {"bridge_stp": "off"},
            }],
        }));
        assert_eq!(state.interface_map()["br0"]["bridge_stp"], json!(false));
    }

    #[test]
    fn a_nameserver_for_an_unknown_interface_is_an_error() {
        let mut warnings = Warnings::default();
        let err = parse_net_config_data(
            &json!({
                "version": 1,
                "config": [{
                    "type": "nameserver",
                    "address": ["8.8.8.8"],
                    "interface": "nope",
                }],
            }),
            Target::Other,
            &mut warnings,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
    }

    #[test]
    fn netplan_v2_is_a_passthrough() {
        let config = json!({"version": 2, "ethernets": {"eth0": {"dhcp4": true}}});
        let mut warnings = Warnings::default();
        let state =
            parse_net_config_data(&config, Target::Netplan, &mut warnings).unwrap();
        assert!(state.is_passthrough());
        assert_eq!(state.config(), &config);
    }

    #[test]
    fn a_config_without_a_version_is_refused() {
        let mut warnings = Warnings::default();
        let err =
            parse_net_config_data(&json!({"config": []}), Target::Other, &mut warnings)
                .unwrap_err();
        assert_eq!(err, Error::NoState);
    }
}
