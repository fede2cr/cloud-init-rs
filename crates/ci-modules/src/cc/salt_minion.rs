//! Port of `cc_salt_minion.py`: install the minion, write its config, key
//! pair and grains, then either run it as a daemon or apply state once.
//!
//! The module writes before it validates -- the packages are installed and
//! `/etc/salt/minion` is on disk before anything looks at what `conf:` holds
//! -- so the machine goes behind [`Host`] and the differential compares the
//! ordered call list as well as the log and the bytes.
//!
//! `salt_minion:` is subscripted without being checked for a type, so a
//! scalar or a list reaches Python's own `TypeError` and `AttributeError`
//! rather than a message the module wrote. Those are reproduced (bugs B101
//! and B102 in `docs/COMPAT.md`), because a half-configured minion is the
//! state upstream leaves behind.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::rsyslog::CmdError;
use super::{dict_get, py_str, sub_option, Args};

const SOURCE: &str = "cc_salt_minion.py";

/// Everything `cc_salt_minion` asks of the machine.
///
/// `&mut self` is for recording rather than state: a [`Fixture`] appends each
/// call to a list so the differential compares the sequence.
pub trait Host {
    /// `cloud.distro.install_packages(pkgs)`.
    fn install_packages(&mut self, packages: &Value) -> Result<(), String>;

    /// `util.ensure_dir(path)`. `umask` is `Some` inside the `util.umask`
    /// block the key pair is written under, where it decides the new
    /// directory's mode.
    fn ensure_dir(&mut self, path: &str, umask: Option<u32>) -> Result<(), String>;

    /// `util.write_file(path, content)`, always at the default mode. The
    /// content is the raw config value: `util.encode_text` runs inside, which
    /// is where a non-string key raises.
    fn write_file(&mut self, path: &str, content: &Value) -> Result<(), String>;

    /// `os.path.isdir(path)`, which picks between the two default key
    /// directories.
    fn is_dir(&mut self, path: &str) -> bool;

    /// `cloud.distro.manage_service(action, service)`.
    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError>;

    /// `subp.subp(argv, capture=False)`, for the masterless `state.apply`.
    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError>;
}

/// `SaltConstants`.
///
/// The `util.is_FreeBSD()` branch is ported but unreachable; see deviation 167.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constants {
    pub pkg_name: String,
    pub srv_name: String,
    pub conf_dir: String,
}

impl Constants {
    /// `SaltConstants(cfg)`.
    ///
    /// # Errors
    /// The `TypeError` a non-container `salt_minion:` raises on the first
    /// `key in cfg`.
    pub fn from_cfg(cfg: &Value) -> Result<Self, String> {
        let mut constants = Self {
            pkg_name: "salt-minion".to_owned(),
            srv_name: "salt-minion".to_owned(),
            conf_dir: "/etc/salt".to_owned(),
        };
        if let Some(value) = sub_option(cfg, "pkg_name")? {
            constants.pkg_name = py_str(&value);
        }
        if let Some(value) = sub_option(cfg, "config_dir")? {
            constants.conf_dir = py_str(&value);
        }
        if let Some(value) = sub_option(cfg, "service_name")? {
            constants.srv_name = py_str(&value);
        }
        Ok(constants)
    }
}

/// `key in yobj`, with no subscript to follow.
fn contains(block: &Value, key: &str) -> Result<bool, String> {
    match block {
        Value::Object(map) => Ok(map.contains_key(key)),
        Value::Array(items) => Ok(items.iter().any(|item| item.as_str() == Some(key))),
        Value::String(text) => Ok(text.contains(key)),
        other => Err(format!(
            "argument of type '{}' is not a container or iterable",
            ci_config::type_name(other)
        )),
    }
}

/// `os.path.join` for the two-component case: an absolute second component
/// replaces the first outright.
fn join_str(base: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_owned();
    }
    if base.is_empty() || base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `util.ensure_dir(path)`'s first move on a path that need not be a string.
///
/// `os.path.isdir` takes a file descriptor, so an `int` -- and a `bool`, which
/// is one -- gets past it and dies in `os.makedirs` instead, with a different
/// message from the one `os.stat` gives everything else.
fn ensure_dir_path(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Bool(_) | Value::Number(_) if is_integral(value) => Err(format!(
            "expected str, bytes or os.PathLike object, not {}",
            ci_config::type_name(value)
        )),
        other => Err(format!(
            "stat: path should be string, bytes, os.PathLike or integer, not {}",
            ci_config::type_name(other)
        )),
    }
}

fn is_integral(value: &Value) -> bool {
    match value {
        Value::Bool(_) => true,
        Value::Number(number) => number.is_i64() || number.is_u64(),
        _ => false,
    }
}

/// `util.encode_text(content)`, which is what turns a non-string key into an
/// `AttributeError` after the file it belongs beside was already opened.
fn encode_text(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Err(format!(
            "'{}' object has no attribute 'encode'",
            ci_config::type_name(other)
        )),
    }
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// Everything upstream lets escape: a `salt_minion:` that is not a mapping, a
/// `conf:` that is not one either, a `pki_dir` or key that is not a string, or
/// a failed write, install or service action.
pub fn handle_with(
    name: &str,
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    let Some(s_cfg) = cfg.get("salt_minion") else {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, no 'salt_minion' key in configuration"
            ),
        );
        return Ok(());
    };

    let constants = Constants::from_cfg(s_cfg)?;

    host.install_packages(&Value::Array(vec![Value::String(
        constants.pkg_name.clone(),
    )]))?;
    host.ensure_dir(&constants.conf_dir, None)?;

    let mut minion_data = None;

    if contains(s_cfg, "conf")? {
        let minion_config = join_str(&constants.conf_dir, "minion");
        let data = dict_get(s_cfg, "conf")?.unwrap_or(Value::Null);
        let dumped = Value::String(ci_core::yamlfmt::dumps(&data));
        host.write_file(&minion_config, &dumped)?;
        minion_data = Some(data);
    }

    if contains(s_cfg, "grains")? {
        let grains_config = join_str(&constants.conf_dir, "grains");
        let grains_data = Value::String(ci_core::yamlfmt::dumps(
            &dict_get(s_cfg, "grains")?.unwrap_or(Value::Null),
        ));
        host.write_file(&grains_config, &grains_data)?;
    }

    if contains(s_cfg, "public_key")? && contains(s_cfg, "private_key")? {
        let mut pki_dir_default = join_str(&constants.conf_dir, "pki/minion");
        if !host.is_dir(&pki_dir_default) {
            pki_dir_default = join_str(&constants.conf_dir, "pki");
        }
        let pki_dir = match dict_get(s_cfg, "pki_dir")? {
            Some(value) => value,
            None => Value::String(pki_dir_default),
        };

        // `util.umask(0o77)`, which only reaches the directory: the two
        // writes that follow chmod to their own mode.
        let pki_dir = ensure_dir_path(&pki_dir)?;
        host.ensure_dir(&pki_dir, Some(0o077))?;
        let pub_name = join_str(&pki_dir, "minion.pub");
        let pem_name = join_str(&pki_dir, "minion.pem");
        host.write_file(
            &pub_name,
            &dict_get(s_cfg, "public_key")?.unwrap_or(Value::Null),
        )?;
        host.write_file(
            &pem_name,
            &dict_get(s_cfg, "private_key")?.unwrap_or(Value::Null),
        )?;
    }

    let masterless = match &minion_data {
        Some(data) if ci_config::option::py_truthy(data) => {
            dict_get(data, "file_client")?
                .as_ref()
                .and_then(Value::as_str)
                == Some("local")
        }
        _ => false,
    };
    let minion_daemon = !masterless;

    host.manage_service(
        if minion_daemon { "enable" } else { "disable" },
        &constants.srv_name,
    )
    .map_err(|error| error.to_string())?;
    host.manage_service(
        if minion_daemon { "restart" } else { "stop" },
        &constants.srv_name,
    )
    .map_err(|error| error.to_string())?;

    if !minion_daemon {
        let argv = ["salt-call", "--local", "state.apply"].map(str::to_owned);
        host.subp(&argv).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The registry entry point.
///
/// # Errors
/// The module failure the stage reports.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut host = Live {
        root: args.root.to_owned(),
        distro: *args.distro,
        system_info: args.system_info.clone(),
    };
    let (name, cfg) = (args.name.to_owned(), args.cfg.clone());
    handle_with(&name, &cfg, &mut host, &mut *args.logger)
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
    distro: ci_distro::Distro,
    system_info: Object,
}

impl Live {
    fn under_root(&self, path: &str) -> std::path::PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }
}

impl Host for Live {
    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        super::rsyslog::install_packages(
            &self.root,
            &self.distro,
            &self.system_info,
            packages,
        )
    }

    fn ensure_dir(&mut self, path: &str, umask: Option<u32>) -> Result<(), String> {
        let target = self.under_root(path);
        if target.is_dir() {
            return Ok(());
        }
        // `os.makedirs` gives every component it creates `0o777` less the
        // umask. The crate forbids `unsafe`, so rather than calling
        // `umask(2)` the mode is applied afterwards to exactly the components
        // that did not exist -- see deviation 168.
        let mut missing = Vec::new();
        let mut walked = target.clone();
        while !walked.is_dir() {
            missing.push(walked.clone());
            match walked.parent() {
                Some(parent) if parent != walked => walked = parent.to_owned(),
                _ => break,
            }
        }
        std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
        if let Some(mask) = umask {
            for made in missing {
                ci_sys::ids::set_mode(&made, 0o777 & !mask)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn write_file(&mut self, path: &str, content: &Value) -> Result<(), String> {
        let content = encode_text(content)?;
        let target = self.under_root(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        ci_sys::atomic::write_file(
            &target,
            content.as_bytes(),
            ci_sys::atomic::WriteOptions {
                mode: 0o644,
                ..ci_sys::atomic::WriteOptions::default()
            },
        )
        .map_err(|error| error.to_string())
    }

    fn is_dir(&mut self, path: &str) -> bool {
        self.under_root(path).is_dir()
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let argv = ci_distro::service::command(&self.distro, action, service, &[])
            .map_err(|key| CmdError {
                command: key,
                ..CmdError::default()
            })?;
        self.subp(&argv)
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let command = ci_config::repr(&Value::Array(
            argv.iter().map(|arg| Value::String(arg.clone())).collect(),
        ));
        let status = ci_sys::subp::Subp::new(argv)
            .inherit_env()
            .passthrough()
            .map_err(|error| CmdError {
                command: command.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        if status.success() {
            return Ok(());
        }
        Err(CmdError {
            command,
            exit_code: status.code(),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Failures, keyed by the call as it is recorded.
    pub failures: Vec<(String, String)>,
    /// Directories that already exist.
    pub dirs: Vec<String>,
    pub calls: Vec<String>,
    /// What each file ended up holding, in the order they were first written.
    pub written: Vec<(String, String)>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    fn failure(&self, call: &str) -> Option<String> {
        self.failures
            .iter()
            .find(|(key, _)| key == call)
            .map(|(_, error)| error.clone())
    }

    fn cmd_failure(&self, call: &str) -> Option<CmdError> {
        self.failure(call).map(|message| CmdError {
            command: call.to_owned(),
            exit_code: Some(1),
            stdout: String::new(),
            stderr: message,
        })
    }
}

impl Host for Fixture {
    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        let call = format!("install_packages {}", ci_config::repr(packages));
        self.record(call.clone());
        self.failure(&call).map_or(Ok(()), Err)
    }

    fn ensure_dir(&mut self, path: &str, umask: Option<u32>) -> Result<(), String> {
        let call = match umask {
            Some(mask) => format!("ensure_dir {path} umask={mask:04o}"),
            None => format!("ensure_dir {path}"),
        };
        self.record(call.clone());
        self.failure(&call).map_or(Ok(()), Err)
    }

    fn write_file(&mut self, path: &str, content: &Value) -> Result<(), String> {
        // `util.write_file` encodes before it opens, so a content that is not
        // a string leaves no trace of the call.
        let content = encode_text(content)?;
        let call = format!("write_file {path}");
        self.record(call.clone());
        if let Some(error) = self.failure(&call) {
            return Err(error);
        }
        match self.written.iter_mut().find(|(key, _)| key == path) {
            Some((_, held)) => content.clone_into(held),
            None => self.written.push((path.to_owned(), content)),
        }
        Ok(())
    }

    fn is_dir(&mut self, path: &str) -> bool {
        self.record(format!("is_dir {path}"));
        self.dirs.iter().any(|dir| dir == path)
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), CmdError> {
        let call = format!("manage_service {action} {service}");
        self.record(call.clone());
        self.cmd_failure(&call).map_or(Ok(()), Err)
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let call = format!("subp {}", argv.join(" "));
        self.record(call.clone());
        self.cmd_failure(&call).map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn run(cfg: &str) -> (Fixture, Vec<String>, Result<(), String>) {
        run_with(cfg, Fixture::default())
    }

    fn run_with(
        cfg: &str,
        mut host: Fixture,
    ) -> (Fixture, Vec<String>, Result<(), String>) {
        let cfg: Object = serde_json::from_str(cfg).unwrap();
        let mut log = Logger::capturing();
        let result = handle_with("cc_salt_minion", &cfg, &mut host, &mut log);
        let captured = log.captured().to_vec();
        (host.clone(), captured, result)
    }

    #[test]
    fn a_config_without_the_key_is_skipped() {
        let (host, log, result) = run(r#"{"other": 1}"#);
        assert!(result.is_ok());
        assert!(host.calls.is_empty());
        assert!(log[0].contains("no 'salt_minion' key in configuration"));
    }

    #[test]
    fn the_defaults_install_and_restart_the_daemon() {
        let (host, _, result) = run(r#"{"salt_minion": {}}"#);
        assert!(result.is_ok());
        assert_eq!(
            host.calls,
            [
                "install_packages ['salt-minion']",
                "ensure_dir /etc/salt",
                "manage_service enable salt-minion",
                "manage_service restart salt-minion",
            ]
        );
    }

    #[test]
    fn the_names_and_directory_can_be_overridden() {
        let cfg = r#"{"salt_minion": {"pkg_name": "py-salt",
            "service_name": "salt_minion", "config_dir": "/usr/local/etc/salt"}}"#;
        let (host, _, result) = run(cfg);
        assert!(result.is_ok());
        assert_eq!(host.calls[0], "install_packages ['py-salt']");
        assert_eq!(host.calls[1], "ensure_dir /usr/local/etc/salt");
        assert_eq!(host.calls[2], "manage_service enable salt_minion");
    }

    #[test]
    fn conf_and_grains_are_written_as_yaml() {
        let (host, _, result) = run(
            r#"{"salt_minion": {"conf": {"master": "m"}, "grains": {"role": "web"}}}"#,
        );
        assert!(result.is_ok());
        assert_eq!(
            host.written,
            [
                (
                    "/etc/salt/minion".to_owned(),
                    "---\nmaster: m\n...\n".to_owned()
                ),
                (
                    "/etc/salt/grains".to_owned(),
                    "---\nrole: web\n...\n".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_key_pair_lands_under_pki_minion_when_it_exists() {
        let host = Fixture {
            dirs: vec!["/etc/salt/pki/minion".to_owned()],
            ..Fixture::default()
        };
        let (host, _, result) = run_with(
            r#"{"salt_minion": {"public_key": "pub", "private_key": "priv"}}"#,
            host,
        );
        assert!(result.is_ok());
        assert!(host
            .calls
            .contains(&"ensure_dir /etc/salt/pki/minion umask=0077".to_owned()));
        assert_eq!(host.written[0].0, "/etc/salt/pki/minion/minion.pub");
        assert_eq!(host.written[1].0, "/etc/salt/pki/minion/minion.pem");
    }

    #[test]
    fn a_key_pair_falls_back_to_pki_when_minion_is_missing() {
        let (host, _, result) =
            run(r#"{"salt_minion": {"public_key": "pub", "private_key": "priv"}}"#);
        assert!(result.is_ok());
        assert_eq!(host.written[0].0, "/etc/salt/pki/minion.pub");
    }

    #[test]
    fn pki_dir_overrides_both_defaults() {
        let cfg = r#"{"salt_minion": {"public_key": "pub", "private_key": "priv",
            "pki_dir": "/keys"}}"#;
        let (host, _, result) = run(cfg);
        assert!(result.is_ok());
        assert_eq!(host.written[0].0, "/keys/minion.pub");
        // The default is still worked out, isdir call and all, before the
        // override is read.
        assert_eq!(host.calls[2], "is_dir /etc/salt/pki/minion");
    }

    #[test]
    fn a_local_file_client_stops_the_daemon_and_applies_state() {
        let (host, _, result) =
            run(r#"{"salt_minion": {"conf": {"file_client": "local"}}}"#);
        assert!(result.is_ok());
        assert_eq!(
            host.calls[2..],
            [
                "write_file /etc/salt/minion",
                "manage_service disable salt-minion",
                "manage_service stop salt-minion",
                "subp salt-call --local state.apply",
            ]
        );
    }

    #[test]
    fn an_empty_conf_leaves_the_daemon_enabled() {
        let (host, _, result) = run(r#"{"salt_minion": {"conf": {}}}"#);
        assert!(result.is_ok());
        assert_eq!(host.calls[3], "manage_service enable salt-minion");
    }

    #[test]
    fn a_scalar_salt_minion_raises_before_anything_runs() {
        let (host, _, result) = run(r#"{"salt_minion": 5}"#);
        assert_eq!(
            result,
            Err("argument of type 'int' is not a container or iterable".to_owned())
        );
        assert!(host.calls.is_empty());
    }

    #[test]
    fn a_string_conf_installs_and_writes_before_it_raises() {
        let (host, _, result) = run(r#"{"salt_minion": "conf"}"#);
        assert_eq!(
            result,
            Err("'str' object has no attribute 'get'".to_owned())
        );
        assert_eq!(host.calls.len(), 2);
    }

    #[test]
    fn a_non_mapping_conf_raises_after_its_yaml_is_on_disk() {
        let (host, _, result) = run(r#"{"salt_minion": {"conf": "hello"}}"#);
        assert_eq!(
            result,
            Err("'str' object has no attribute 'get'".to_owned())
        );
        assert_eq!(host.written[0].1, "--- hello\n...\n");
    }

    #[test]
    fn a_non_string_pki_dir_raises_before_the_directory_is_made() {
        let cfg =
            r#"{"salt_minion": {"public_key": "p", "private_key": "q", "pki_dir": 5}}"#;
        let (host, _, result) = run(cfg);
        assert_eq!(
            result,
            Err("expected str, bytes or os.PathLike object, not int".to_owned())
        );
        assert!(!host.calls.iter().any(|call| call.contains("umask")));
    }

    #[test]
    fn a_null_pki_dir_dies_in_stat_rather_than_mkdir() {
        let cfg = r#"{"salt_minion": {"public_key": "p", "private_key": "q", "pki_dir": null}}"#;
        let (_, _, result) = run(cfg);
        assert_eq!(
            result,
            Err("stat: path should be string, bytes, os.PathLike or integer, not NoneType"
                .to_owned())
        );
    }

    #[test]
    fn a_non_string_key_raises_after_the_directory_is_made() {
        let cfg = r#"{"salt_minion": {"public_key": 5, "private_key": "q"}}"#;
        let (host, _, result) = run(cfg);
        assert_eq!(
            result,
            Err("'int' object has no attribute 'encode'".to_owned())
        );
        assert!(host.calls.iter().any(|call| call.contains("umask=0077")));
        assert_eq!(
            host.calls.last().unwrap(),
            "ensure_dir /etc/salt/pki umask=0077"
        );
        assert!(host.written.is_empty());
    }
}
