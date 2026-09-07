//! `cloudinit.net.renderer`: what every network renderer has in common.
//!
//! Upstream's is an `abc.ABC` with one abstract method and one shared static
//! helper. The shape survives the port; what does not is the assumption that
//! a renderer writes straight to `/`, so `render_network_state` takes the
//! target root explicitly rather than defaulting it inside each renderer.

use std::path::Path;

use ci_config::{Object, Value};

use crate::netplan;
use crate::state::NetworkState;
use crate::udev::generate_udev_rule;

/// Why a renderer could not produce a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The renderer exists upstream but has no body here yet.
    NotImplemented(&'static str),
    /// The rendered configuration could not be written out.
    Write(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented(name) => {
                write!(f, "network renderer '{name}' is not implemented yet")
            }
            Self::Write(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

/// `renderer.Renderer`: turns a [`NetworkState`] into files under a root.
pub trait Renderer {
    /// The `NAME_TO_RENDERER` key this renderer was selected under.
    fn name(&self) -> &'static str;

    /// `Renderer.render_network_state`.
    ///
    /// `target` is upstream's `target=`, the root to write beneath; `None` is
    /// its default of `/`.
    fn render_network_state(
        &self,
        state: &NetworkState,
        target: Option<&Path>,
    ) -> Result<(), Error>;
}

/// `renderer.filter_by_type`.
#[must_use]
pub fn filter_by_type<'a>(
    interfaces: &[&'a Value],
    match_type: &str,
) -> Vec<&'a Value> {
    interfaces
        .iter()
        .copied()
        .filter(|iface| iface.get("type").and_then(Value::as_str) == Some(match_type))
        .collect()
}

/// `renderer.filter_by_attr`: present *and* truthy, as Python reads it.
#[must_use]
pub fn filter_by_attr<'a>(
    interfaces: &[&'a Value],
    match_name: &str,
) -> Vec<&'a Value> {
    interfaces
        .iter()
        .copied()
        .filter(|iface| {
            iface
                .get(match_name)
                .is_some_and(ci_config::option::py_truthy)
        })
        .collect()
}

/// `Renderer._render_persistent_net`: udev rules pinning names to MACs.
///
/// Only physical interfaces get one, and only those with both a name and a
/// non-empty MAC — a virtual device has no address to match on.
#[must_use]
pub fn render_persistent_net(state: &NetworkState) -> String {
    let mut out = String::new();
    for iface in filter_by_type(&state.interfaces(), "physical") {
        let Some(name) = iface.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mac = iface.get("mac_address").and_then(Value::as_str);
        let Some(mac) = mac.filter(|mac| !mac.is_empty()) else {
            continue;
        };
        let driver = iface.get("driver").and_then(Value::as_str);
        out.push_str(&generate_udev_rule(name, mac, driver));
    }
    out
}

/// The netplan renderer, and so far the only one with a body.
#[derive(Debug, Clone)]
pub struct Netplan {
    /// `renderer_configs["netplan"]["netplan_path"]`.
    pub path: String,
    /// `renderer_configs["netplan"]["netplan_header"]`.
    pub header: String,
    /// `renderer_configs["netplan"]["features"]`.
    pub features: netplan::Features,
}

impl Netplan {
    /// Build one from a distro's `renderer_configs["netplan"]`.
    ///
    /// The defaults are upstream's own: the class attribute for the path, and
    /// no header at all, which is what a distro that sets only `netplan_path`
    /// gets.
    #[must_use]
    pub fn from_config(config: &Object) -> Self {
        Self {
            path: config
                .get("netplan_path")
                .and_then(Value::as_str)
                .unwrap_or(netplan::NETPLAN_FILE)
                .to_owned(),
            header: config
                .get("netplan_header")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            features: netplan::Features::default(),
        }
    }

    /// The file content this renderer would write, and the warnings raised
    /// getting there.
    #[must_use]
    pub fn content(&self, state: &NetworkState) -> (String, netplan::Warnings) {
        let mut warnings = netplan::Warnings::default();
        let body = netplan::render_content(state, self.features, &mut warnings);
        (netplan::render_with_header(&body, &self.header), warnings)
    }
}

impl Renderer for Netplan {
    fn name(&self) -> &'static str {
        "netplan"
    }

    fn render_network_state(
        &self,
        state: &NetworkState,
        target: Option<&Path>,
    ) -> Result<(), Error> {
        let (content, _) = self.content(state);
        let relative = self.path.trim_start_matches('/');
        let path = match target {
            Some(root) => root.join(relative),
            None => Path::new("/").join(relative),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| Error::Write(format!("{}: {err}", parent.display())))?;
        }
        std::fs::write(&path, content)
            .map_err(|err| Error::Write(format!("{}: {err}", path.display())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state_with(interfaces: &Value) -> NetworkState {
        let mut warnings = crate::state::Warnings::default();
        crate::state::parse_net_config_data(
            &json!({"version": 1, "config": interfaces}),
            crate::state::Target::Other,
            &mut warnings,
        )
        .unwrap()
    }

    #[test]
    fn only_physical_interfaces_with_a_mac_get_a_rule() {
        let state = state_with(&json!([
            {"type": "physical", "name": "eth0", "mac_address": "aa:bb:cc:dd:ee:ff"},
            {"type": "physical", "name": "eth1"},
            {"type": "bond", "name": "bond0", "mac_address": "aa:bb:cc:dd:ee:00",
             "bond_interfaces": ["eth0"]},
        ]));
        assert_eq!(
            render_persistent_net(&state),
            "SUBSYSTEM==\"net\", ACTION==\"add\", DRIVERS==\"?*\", \
             ATTR{address}==\"aa:bb:cc:dd:ee:ff\", NAME=\"eth0\"\n"
        );
    }

    #[test]
    fn nothing_physical_means_no_rules() {
        let state = state_with(&json!([]));
        assert_eq!(render_persistent_net(&state), "");
    }

    #[test]
    fn netplan_takes_its_path_and_header_from_the_distro() {
        let ubuntu: Object = serde_json::from_value(json!({
            "netplan_path": "/etc/netplan/50-cloud-init.yaml",
            "netplan_header": "# hi\n",
        }))
        .unwrap();
        let renderer = Netplan::from_config(&ubuntu);
        assert_eq!(renderer.path, "/etc/netplan/50-cloud-init.yaml");
        assert_eq!(renderer.header, "# hi\n");

        // A distro with no netplan entry at all still renders somewhere.
        let bare = Netplan::from_config(&Object::new());
        assert_eq!(bare.path, netplan::NETPLAN_FILE);
        assert_eq!(bare.header, "");
    }

    #[test]
    fn rendering_writes_beneath_the_target_root() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&json!([
            {"type": "physical", "name": "eth0", "subnets": [{"type": "dhcp4"}]},
        ]));
        let renderer = Netplan::from_config(&Object::new());
        renderer
            .render_network_state(&state, Some(dir.path()))
            .unwrap();
        let written =
            std::fs::read_to_string(dir.path().join("etc/netplan/50-cloud-init.yaml"))
                .unwrap();
        assert!(written.contains("eth0"), "{written}");
    }
}
