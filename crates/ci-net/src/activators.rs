//! `cloudinit.net.activators`: making the rendered config take effect *now*.
//!
//! Rendering writes a file the next boot would read. An activator is what makes
//! the running kernel adopt it — `netplan apply`, a `systemd-networkd` restart,
//! `nmcli connection up`, `ifup`. Every one of them runs a program, so this
//! module changes the machine, the way [`crate::netops`] and
//! [`crate::ephemeral`] do and the rest of the crate does not.
//!
//! Which activator is chosen is *independent* of which renderer was chosen:
//! `DEFAULT_PRIORITY` here puts `eni` ahead of `netplan`, so a machine that
//! renders netplan but also has `ifup` and `/etc/network/interfaces` is brought
//! up with `ifup`. That is upstream's behaviour and all five are ported so it
//! can be reproduced.

use ci_config::Value;
use ci_log::Logger;
use ci_sys::subp;

use crate::renderers::py_list;
use crate::state::NetworkState;

/// Upstream logs every line in this module against `activators.py`.
const SRC: &str = "activators.py";

/// `activators.DEFAULT_PRIORITY`.
pub const DEFAULT_PRIORITY: &[&str] =
    &["eni", "netplan", "network-manager", "networkd", "ifconfig"];

/// Why [`select`] could not name an activator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `ValueError`: the priority list names something not in
    /// `NAME_TO_ACTIVATOR`. Upstream lets this one escape `apply_network_config`
    /// and fail the stage.
    Unknown(Vec<String>),
    /// `NoActivatorException`, which the caller catches and warns about.
    NoActivator(Vec<String>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(names) => write!(
                f,
                "Unknown activators provided in priority list: {}",
                py_list(names)
            ),
            Self::NoActivator(priority) => write!(
                f,
                "No available network activators found. Searched through list: {}",
                py_list(priority)
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Why an activator could not wait for the network.
#[derive(Debug)]
pub enum WaitError {
    /// The base class's `raise NotImplementedError()`, whose `str()` is empty.
    NotImplemented,
    /// `systemd-networkd-wait-online` ran and failed.
    Process(String),
    /// It could not be run at all.
    Subp(subp::Error),
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `str(NotImplementedError())` is the empty string, and the caller
            // interpolates it into a message with `%s`.
            Self::NotImplemented => Ok(()),
            Self::Process(message) => f.write_str(message),
            Self::Subp(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WaitError {}

/// `subp.ProcessExecutionError`, rendered the way its `__str__` reads.
///
/// `wait_for_network` is the only place in the port where a raw
/// `ProcessExecutionError` reaches a log line rather than being caught and
/// reworded, so the template lives here instead of in [`subp::Error`], whose
/// own `Display` is the port's idiom everywhere else.
///
/// `description` and `reason` are always their defaults on this path, and
/// `stdout`/`stderr` keep the reindenting upstream applies to them (B47): a
/// non-empty stream has every line after the first shifted right by eight
/// spaces, while an empty one stays empty rather than becoming `-`, because
/// `subp` hands over `""` and not `None`.
fn process_execution_error(argv: &[&str], out: &subp::Output) -> String {
    fn indent(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw)
            .trim_end_matches('\n')
            .replace('\n', "\n        ")
    }
    format!(
        "Unexpected error while running command.\n\
         Command: {}\n\
         Exit code: {}\n\
         Reason: -\n\
         Stdout: {}\n\
         Stderr: {}",
        py_list(&argv.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>()),
        // A child killed by a signal never reached `exit_code`, so upstream
        // leaves the placeholder in.
        out.code
            .map_or_else(|| "-".to_owned(), |code| code.to_string()),
        indent(&out.stdout),
        indent(&out.stderr),
    )
}

/// `activators.NetworkActivator`.
///
/// `bring_up_interface`/`bring_down_interface` cannot fail beyond returning
/// `false`: every one of them goes through [`alter_interface`], which swallows
/// `ProcessExecutionError`. The `bring_up_interfaces` family can, because
/// `NetworkManagerActivator` asks systemd a question outside that guard.
pub trait NetworkActivator: std::fmt::Debug {
    /// The `NAME_TO_ACTIVATOR` key.
    fn name(&self) -> &'static str;

    /// `repr()` of the upstream class, which `select_activator` logs.
    fn py_repr(&self) -> &'static str;

    fn available(&self) -> bool;

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool;

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool;

    fn bring_up_interfaces(
        &self,
        devices: &[String],
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        Ok(devices
            .iter()
            .all(|device| self.bring_up_interface(device, log)))
    }

    fn bring_up_all_interfaces(
        &self,
        state: &NetworkState,
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        self.bring_up_interfaces(&interface_names(state), log)
    }

    fn wait_for_network(&self, log: &mut Logger) -> Result<(), WaitError> {
        let _ = log;
        Err(WaitError::NotImplemented)
    }
}

/// `[i["name"] for i in network_state.iter_interfaces()]`.
fn interface_names(state: &NetworkState) -> Vec<String> {
    state
        .interfaces()
        .iter()
        .filter_map(|iface| iface.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

/// `activators._alter_interface`: run a command and standardise the reporting.
///
/// A non-empty stderr from a *successful* command is still reported, because
/// `ifup` and `nmcli` warn that way; `netplan apply` chatters there on every
/// run, which is why it alone passes `warn_on_stderr=False`.
fn alter_interface(argv: &[&str], warn_on_stderr: bool, log: &mut Logger) -> bool {
    let Ok(out) = subp::Subp::new(argv).check() else {
        logexc(
            log,
            &format!(
                "Running interface command {} failed",
                py_list(&argv.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
            ),
        );
        return false;
    };
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.is_empty() {
        let msg = format!("Received stderr output: {err}");
        if warn_on_stderr {
            log.warning(SRC, &msg);
        } else {
            log.debug(SRC, &msg);
        }
    }
    true
}

/// `util.logexc`: the message at warning level, then again at debug where
/// upstream attaches the traceback this port does not have.
fn logexc(log: &mut Logger, msg: &str) {
    log.warning(SRC, msg);
    log.debug(SRC, msg);
}

/// `activators.IfUpDownActivator`.
#[derive(Debug)]
pub struct IfUpDown;

impl NetworkActivator for IfUpDown {
    fn name(&self) -> &'static str {
        "eni"
    }

    fn py_repr(&self) -> &'static str {
        "<class 'cloudinit.net.activators.IfUpDownActivator'>"
    }

    fn available(&self) -> bool {
        crate::renderers::eni_available()
    }

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_interface(&["ifup", device], true, log)
    }

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_interface(&["ifdown", device], true, log)
    }
}

/// `activators.IfConfigActivator`.
#[derive(Debug)]
pub struct IfConfig;

impl NetworkActivator for IfConfig {
    fn name(&self) -> &'static str {
        "ifconfig"
    }

    fn py_repr(&self) -> &'static str {
        "<class 'cloudinit.net.activators.IfConfigActivator'>"
    }

    fn available(&self) -> bool {
        subp::which_in("ifconfig", &["/sbin"]).is_some()
    }

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_interface(&["ifconfig", device, "up"], true, log)
    }

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_interface(&["ifconfig", device, "down"], true, log)
    }
}

/// `activators.NetworkManagerActivator`.
#[derive(Debug)]
pub struct NetworkManager;

impl NetworkActivator for NetworkManager {
    fn name(&self) -> &'static str {
        "network-manager"
    }

    fn py_repr(&self) -> &'static str {
        "<class 'cloudinit.net.activators.NetworkManagerActivator'>"
    }

    fn available(&self) -> bool {
        crate::renderers::network_manager_available()
    }

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool {
        let Some(filename) = conn_filename(device) else {
            log.warning(
                SRC,
                "Unable to find an interface config file. \
                 Unable to bring up interface.",
            );
            return false;
        };
        // Loading by filename is preferred; if that fails NetworkManager is
        // asked to reload everything and the device is named instead.
        let cmd = if alter_interface(
            &["nmcli", "connection", "load", &filename],
            true,
            log,
        ) {
            vec!["nmcli", "connection", "up", "filename", &filename]
        } else {
            alter_interface(&["nmcli", "connection", "reload"], true, log);
            vec!["nmcli", "connection", "up", "ifname", device]
        };
        alter_interface(&cmd, true, log)
    }

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_interface(&["nmcli", "device", "disconnect", device], true, log)
    }

    fn bring_up_interfaces(
        &self,
        devices: &[String],
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        // Not guarded: upstream lets this one fail the stage.
        let out = subp::Subp::new([
            "systemctl",
            "show",
            "--property=SubState",
            "NetworkManager.service",
        ])
        .check()?;
        let state = out.stdout_lossy().trim_end().to_owned();
        if state != "SubState=running" {
            log.warning(
                SRC,
                &format!(
                    "Expected NetworkManager SubState=running, but detected: {state}"
                ),
            );
        }
        Ok(alter_interface(
            &[
                "systemctl",
                "try-reload-or-restart",
                "NetworkManager.service",
            ],
            true,
            log,
        ) && devices
            .iter()
            .all(|device| self.bring_up_interface(device, log)))
    }
}

/// `network_manager.conn_filename`: the config file `NetworkManager` would read
/// for `device`, or `None` if there is not one.
fn conn_filename(device: &str) -> Option<String> {
    let conn_file = format!(
        "/etc/NetworkManager/system-connections/cloud-init-{device}.nmconnection"
    );
    let conn_file = if std::path::Path::new(&conn_file).is_file() {
        conn_file
    } else if crate::renderers::available_nm_ifcfg_rh() {
        // The ifcfg-rh plugin lets NetworkManager read sysconfig files too.
        format!("/etc/sysconfig/network-scripts/ifcfg-{device}")
    } else {
        conn_file
    };
    std::path::Path::new(&conn_file)
        .is_file()
        .then_some(conn_file)
}

/// `activators.NetplanActivator`.
///
/// Every entry point is the same `netplan apply`: netplan has no notion of one
/// interface, so bringing one up brings all of them up.
#[derive(Debug)]
pub struct Netplan;

/// `NetplanActivator.NETPLAN_CMD`.
const NETPLAN_CMD: &[&str] = &["netplan", "apply"];

impl Netplan {
    fn apply(log: &mut Logger) -> bool {
        alter_interface(NETPLAN_CMD, false, log)
    }

    fn log_whole_config(log: &mut Logger) {
        log.debug(
            SRC,
            "Calling 'netplan apply' rather than altering individual interfaces",
        );
    }
}

impl NetworkActivator for Netplan {
    fn name(&self) -> &'static str {
        "netplan"
    }

    fn py_repr(&self) -> &'static str {
        "<class 'cloudinit.net.activators.NetplanActivator'>"
    }

    fn available(&self) -> bool {
        crate::renderers::netplan_available()
    }

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool {
        let _ = device;
        Self::log_whole_config(log);
        Self::apply(log)
    }

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool {
        let _ = device;
        Self::log_whole_config(log);
        Self::apply(log)
    }

    fn bring_up_interfaces(
        &self,
        devices: &[String],
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        let _ = devices;
        Self::log_whole_config(log);
        Ok(Self::apply(log))
    }

    fn bring_up_all_interfaces(
        &self,
        state: &NetworkState,
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        // Note there is no debug line on this path upstream, unlike the other
        // three, and no interface list is consulted at all.
        let _ = state;
        Ok(Self::apply(log))
    }

    fn wait_for_network(&self, log: &mut Logger) -> Result<(), WaitError> {
        if crate::renderers::network_manager_available() {
            log.debug(SRC, "NetworkManager is enabled, skipping networkd wait");
            return Ok(());
        }
        Networkd.wait_for_network(log)
    }
}

/// `activators.NetworkdActivator`.
#[derive(Debug)]
pub struct Networkd;

impl NetworkActivator for Networkd {
    fn name(&self) -> &'static str {
        "networkd"
    }

    fn py_repr(&self) -> &'static str {
        "<class 'cloudinit.net.activators.NetworkdActivator'>"
    }

    fn available(&self) -> bool {
        crate::renderers::networkd_available()
    }

    fn bring_up_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_callable(crate::netops::link_up(device, None), log)
    }

    fn bring_down_interface(&self, device: &str, log: &mut Logger) -> bool {
        alter_callable(crate::netops::link_down(device, None), log)
    }

    fn bring_up_all_interfaces(
        &self,
        state: &NetworkState,
        log: &mut Logger,
    ) -> Result<bool, subp::Error> {
        let _ = state;
        Ok(alter_interface(
            &[
                "systemctl",
                "restart",
                "systemd-networkd",
                "systemd-resolved",
            ],
            true,
            log,
        ))
    }

    fn wait_for_network(&self, log: &mut Logger) -> Result<(), WaitError> {
        let _ = log;
        let argv = ["systemctl", "start", "systemd-networkd-wait-online.service"];
        match subp::Subp::new(argv).run() {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(WaitError::Process(process_execution_error(&argv, &out))),
            Err(err) => Err(WaitError::Subp(err)),
        }
    }
}

/// `activators._alter_interface_callable` for the `netops` entry points.
///
/// The `netops` wrappers have already thrown their stdout and stderr away, so
/// the success branch has no stderr to report. The failure branch names the
/// argv the way upstream's `e.cmd` does.
fn alter_callable(result: Result<(), crate::netops::Error>, log: &mut Logger) -> bool {
    match result {
        Ok(()) => true,
        Err(err) => {
            logexc(
                log,
                &format!("Running interface command {} failed", py_list(&err.argv)),
            );
            false
        }
    }
}

/// `activators.NAME_TO_ACTIVATOR`.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static dyn NetworkActivator> {
    match name {
        "eni" => Some(&IfUpDown),
        "netplan" => Some(&Netplan),
        "network-manager" => Some(&NetworkManager),
        "networkd" => Some(&Networkd),
        "ifconfig" => Some(&IfConfig),
        _ => None,
    }
}

/// `activators.search_activator`: the first activator in `priority` this
/// machine has, or `None`.
pub fn search(
    priority: Option<&[String]>,
) -> Result<Option<&'static dyn NetworkActivator>, Error> {
    let default: Vec<String> =
        DEFAULT_PRIORITY.iter().map(|s| (*s).to_owned()).collect();
    let priority = priority.unwrap_or(&default);

    let unknown: Vec<String> = priority
        .iter()
        .filter(|name| by_name(name).is_none())
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(Error::Unknown(unknown));
    }

    Ok(priority
        .iter()
        .filter_map(|name| by_name(name))
        .find(|activator| activator.available()))
}

/// `activators.select_activator`: the one activator this machine will use.
pub fn select(
    priority: Option<&[String]>,
    log: &mut Logger,
) -> Result<&'static dyn NetworkActivator, Error> {
    let selected = search(priority)?.ok_or_else(|| {
        Error::NoActivator(priority.map_or_else(
            || DEFAULT_PRIORITY.iter().map(|s| (*s).to_owned()).collect(),
            <[String]>::to_vec,
        ))
    })?;
    log.debug(
        SRC,
        &format!(
            "Using selected activator: {} from priority: {}",
            selected.py_repr(),
            match priority {
                Some(list) => py_list(list),
                None => py_list(
                    &DEFAULT_PRIORITY
                        .iter()
                        .map(|s| (*s).to_owned())
                        .collect::<Vec<_>>()
                ),
            }
        ),
    );
    Ok(selected)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_priority_list_is_upstreams() {
        assert_eq!(
            DEFAULT_PRIORITY,
            ["eni", "netplan", "network-manager", "networkd", "ifconfig"]
        );
        for name in DEFAULT_PRIORITY {
            assert_eq!(by_name(name).map(NetworkActivator::name), Some(*name));
        }
    }

    #[test]
    fn an_unknown_name_is_reported_the_way_python_reports_it() {
        let err = search(Some(&names(&["netplan", "nosuch", "worse"]))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Unknown activators provided in priority list: ['nosuch', 'worse']"
        );
    }

    #[test]
    fn an_empty_priority_list_finds_nothing() {
        let mut log = Logger::silent();
        assert!(search(Some(&[])).unwrap().is_none());
        let err = select(Some(&[]), &mut log).unwrap_err();
        assert_eq!(
            err.to_string(),
            "No available network activators found. Searched through list: []"
        );
    }

    #[test]
    fn the_default_list_is_named_in_the_not_found_message() {
        // Only reachable on a machine with none of the five, so the message is
        // built directly rather than by provoking it.
        let err = Error::NoActivator(names(DEFAULT_PRIORITY));
        assert_eq!(
            err.to_string(),
            "No available network activators found. Searched through list: \
             ['eni', 'netplan', 'network-manager', 'networkd', 'ifconfig']"
        );
    }

    #[test]
    fn a_failed_command_is_logged_and_reported_false() {
        let mut log = Logger::silent();
        assert!(!alter_interface(
            &["/nonexistent/ifup", "eth0"],
            true,
            &mut log
        ));
    }

    #[test]
    fn a_not_implemented_wait_stringifies_to_nothing() {
        // Ubuntu's `wait_for_network` interpolates the exception with `%s`.
        assert_eq!(WaitError::NotImplemented.to_string(), "");
        let mut log = Logger::silent();
        assert!(matches!(
            IfUpDown.wait_for_network(&mut log),
            Err(WaitError::NotImplemented)
        ));
    }

    #[test]
    fn a_failed_wait_reads_like_a_process_execution_error() {
        let out = subp::Output {
            code: Some(1),
            stdout: Vec::new(),
            stderr: b"first\nsecond\n".to_vec(),
            truncated: false,
        };
        assert_eq!(
            process_execution_error(&["systemctl", "start", "x.service"], &out),
            "Unexpected error while running command.\n\
             Command: ['systemctl', 'start', 'x.service']\n\
             Exit code: 1\n\
             Reason: -\n\
             Stdout: \n\
             Stderr: first\n        second"
        );
    }

    #[test]
    fn interface_names_come_from_the_state_in_order() {
        let cfg = serde_json::json!({
            "version": 1,
            "config": [
                {"type": "physical", "name": "eth1"},
                {"type": "physical", "name": "eth0"},
            ],
        });
        let mut warnings = crate::state::Warnings::default();
        let state = crate::state::parse_net_config_data(
            &cfg,
            crate::state::Target::Netplan,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(interface_names(&state), names(&["eth1", "eth0"]));
    }
}
