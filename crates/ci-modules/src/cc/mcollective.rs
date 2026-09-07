//! Port of `cc_mcollective.py`: install mcollective, fold `conf:` into
//! `/etc/mcollective/server.cfg` and restart the daemon.
//!
//! The interesting part is not the module, which is forty lines, but the file
//! it rewrites: `server.cfg` goes through `configobj`, and everything
//! `configobj` does to a file on the way through -- see
//! [`ci_config::configobj`] -- happens to the operator's file whether or not
//! `conf:` mentions any of it. A value with a comma comes back as a list, the
//! space before an inline comment is dropped, quotes are re-derived from the
//! value rather than preserved, and every scalar is written before every
//! section.
//!
//! Three upstream behaviours are reproduced rather than corrected (bugs B103
//! to B105 in `docs/COMPAT.md`):
//!
//! - `mcollective:` is subscripted without a type check, so a scalar takes the
//!   stage down and a string half-configures the machine;
//! - `conf:` is handed to `dict.items()` without a type check, after the
//!   package has been installed and the old file read;
//! - a single non-ASCII byte anywhere in `server.cfg`, or in any value
//!   `conf:` sets, parses fine and then makes the write raise
//!   `UnicodeEncodeError` -- with `server.cfg.old` already overwritten.

use ci_config::configobj::ConfigObj;
use ci_config::{Object, Value};
use ci_log::Logger;

use super::rsyslog::CmdError;
use super::{py_str, sub_option, Args};

const SOURCE: &str = "cc_mcollective.py";

/// `PUBCERT_FILE`.
pub const PUBCERT_FILE: &str = "/etc/mcollective/ssl/server-public.pem";

/// `PRICERT_FILE`.
pub const PRICERT_FILE: &str = "/etc/mcollective/ssl/server-private.pem";

/// `SERVER_CFG`.
pub const SERVER_CFG: &str = "/etc/mcollective/server.cfg";

/// `errno.ENOENT`.
const ENOENT: i32 = 2;

/// An `OSError` from the two calls the module wraps in `except IOError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsError {
    /// `e.errno`, which is the only field the module looks at.
    pub errno: i32,
    /// `str(e)`, which is what the stage reports.
    pub display: String,
}

impl OsError {
    /// The `FileNotFoundError` an absent file raises.
    #[must_use]
    pub fn not_found(path: &str) -> Self {
        Self {
            errno: ENOENT,
            display: format!("[Errno 2] No such file or directory: '{path}'"),
        }
    }
}

/// Everything `cc_mcollective` asks of the machine.
///
/// `&mut self` is for recording rather than state: a [`Fixture`] appends each
/// call to a list so the differential compares the sequence.
pub trait Host {
    /// `cloud.distro.install_packages(pkgs)`.
    fn install_packages(&mut self, packages: &Value) -> Result<(), String>;

    /// `util.load_binary_file(path, quiet=False)`.
    fn load_binary_file(&mut self, path: &str) -> Result<Vec<u8>, OsError>;

    /// `util.write_file(path, content, mode=)`.
    fn write_file(
        &mut self,
        path: &str,
        content: &[u8],
        mode: u32,
    ) -> Result<(), String>;

    /// `util.copy(src, dest)`.
    fn copy(&mut self, src: &str, dest: &str) -> Result<(), OsError>;

    /// `subp.subp(argv, capture=False)`.
    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError>;
}

/// `util.encode_text(content)`, which turns a non-string `public-cert:` into
/// an `AttributeError`.
fn encode_text(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        other => Err(format!(
            "'{}' object has no attribute 'encode'",
            ci_config::type_name(other)
        )),
    }
}

/// `configure`.
fn configure<H: Host + ?Sized>(
    config: &Value,
    host: &mut H,
    log: &mut Logger,
) -> Result<(), String> {
    let mut server = match host.load_binary_file(SERVER_CFG) {
        Ok(old_contents) => {
            ConfigObj::parse(&old_contents).map_err(|error| error.to_string())?
        }
        Err(error) if error.errno == ENOENT => {
            log.debug(
                SOURCE,
                &format!(
                    "Did not find file {SERVER_CFG} (starting with an empty config)"
                ),
            );
            ConfigObj::new()
        }
        Err(error) => return Err(error.display),
    };

    let Value::Object(map) = config else {
        return Err(format!(
            "'{}' object has no attribute 'items'",
            ci_config::type_name(config)
        ));
    };
    for (cfg_name, cfg) in map {
        if cfg_name == "public-cert" {
            let content = encode_text(cfg)?;
            host.write_file(PUBCERT_FILE, content.as_bytes(), 0o644)?;
            server.set_str("plugin.ssl_server_public", PUBCERT_FILE);
            server.set_str("securityprovider", "ssl");
        } else if cfg_name == "private-cert" {
            let content = encode_text(cfg)?;
            host.write_file(PRICERT_FILE, content.as_bytes(), 0o600)?;
            server.set_str("plugin.ssl_server_private", PRICERT_FILE);
            server.set_str("securityprovider", "ssl");
        } else if let Value::String(text) = cfg {
            server.set_str(cfg_name, text);
        } else if let Value::Object(options) = cfg {
            if !server.section_names().iter().any(|name| name == cfg_name) {
                server.set(cfg_name, &Value::Object(Object::new()));
            }
            if let Some(section) = server.section_mut(cfg_name) {
                for (option, value) in options {
                    section.set(option, value);
                }
            }
        } else {
            // `str(cfg)`, which makes a list a string rather than a list
            // value.
            server.set_str(cfg_name, &py_str(cfg));
        }
    }

    match host.copy(SERVER_CFG, &format!("{SERVER_CFG}.old")) {
        Ok(()) => {}
        Err(error) if error.errno == ENOENT => {}
        Err(error) => return Err(error.display),
    }

    let contents = server.write().map_err(|error| error.to_string())?;
    host.write_file(SERVER_CFG, &contents, 0o644)
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// Everything upstream lets escape: an `mcollective:` that is not a mapping,
/// a `conf:` that is not one either, a `server.cfg` that does not parse or
/// does not encode, a failed install, copy or write, and the restart.
pub fn handle_with<H: Host + ?Sized>(
    name: &str,
    cfg: &Object,
    host: &mut H,
    log: &mut Logger,
) -> Result<(), String> {
    let Some(mcollective_cfg) = cfg.get("mcollective") else {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, no 'mcollective' key in configuration"
            ),
        );
        return Ok(());
    };

    host.install_packages(&Value::Array(vec![Value::String("mcollective".into())]))?;

    if let Some(conf) = sub_option(mcollective_cfg, "conf")? {
        configure(&conf, host, log)?;
    }

    host.subp(&[
        "service".to_owned(),
        "mcollective".to_owned(),
        "restart".to_owned(),
    ])
    .map_err(|error| error.to_string())
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

/// `os.strerror(code)` for the codes the module can be handed.
fn strerror(errno: i32) -> String {
    match errno {
        ENOENT => "No such file or directory".to_owned(),
        13 => "Permission denied".to_owned(),
        20 => "Not a directory".to_owned(),
        21 => "Is a directory".to_owned(),
        _ => std::io::Error::from_raw_os_error(errno).to_string(),
    }
}

/// Turn a filesystem error into the `OSError` Python would have raised.
fn os_error(path: &str, error: &std::io::Error) -> OsError {
    let errno = error.raw_os_error().unwrap_or(0);
    OsError {
        errno,
        display: format!("[Errno {errno}] {}: '{path}'", strerror(errno)),
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

    fn load_binary_file(&mut self, path: &str) -> Result<Vec<u8>, OsError> {
        std::fs::read(self.under_root(path)).map_err(|error| os_error(path, &error))
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &[u8],
        mode: u32,
    ) -> Result<(), String> {
        let target = self.under_root(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        ci_sys::atomic::write_file(
            &target,
            content,
            ci_sys::atomic::WriteOptions {
                mode,
                ..ci_sys::atomic::WriteOptions::default()
            },
        )
        .map_err(|error| error.to_string())
    }

    fn copy(&mut self, src: &str, dest: &str) -> Result<(), OsError> {
        std::fs::copy(self.under_root(src), self.under_root(dest))
            .map(|_| ())
            .map_err(|error| os_error(src, &error))
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

/// The recording machine the differential drives.
#[derive(Debug, Default)]
pub struct Fixture {
    /// Every call, in order.
    pub calls: Vec<String>,
    /// Path and content of every `util.write_file`.
    pub written: Vec<(String, String)>,
    /// What `load_binary_file` should answer with, by path.
    pub files: Vec<(String, String)>,
    /// Paths whose `load_binary_file` or `copy` raises something other than
    /// `ENOENT`, with the errno to raise.
    pub failures: Vec<(String, i32)>,
    /// Calls that should fail, by recorded call text, with the message.
    pub errors: Vec<(String, String)>,
}

impl Fixture {
    fn fail(&self, call: &str) -> Option<String> {
        self.errors
            .iter()
            .find(|(key, _)| key == call)
            .map(|(_, message)| message.clone())
    }
}

impl Host for Fixture {
    fn install_packages(&mut self, packages: &Value) -> Result<(), String> {
        let call = format!("install_packages {}", ci_config::repr(packages));
        self.calls.push(call.clone());
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn load_binary_file(&mut self, path: &str) -> Result<Vec<u8>, OsError> {
        self.calls.push(format!("load_binary_file {path}"));
        if let Some((_, errno)) = self.failures.iter().find(|(key, _)| key == path) {
            return Err(OsError {
                errno: *errno,
                display: format!("[Errno {errno}] {}: '{path}'", strerror(*errno)),
            });
        }
        self.files
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, content)| content.as_bytes().to_vec())
            .ok_or_else(|| OsError::not_found(path))
    }

    fn write_file(
        &mut self,
        path: &str,
        content: &[u8],
        mode: u32,
    ) -> Result<(), String> {
        let call = format!("write_file {path} mode={mode:04o}");
        self.calls.push(call.clone());
        self.written.push((
            path.to_owned(),
            String::from_utf8_lossy(content).into_owned(),
        ));
        self.fail(&call).map_or(Ok(()), Err)
    }

    fn copy(&mut self, src: &str, dest: &str) -> Result<(), OsError> {
        self.calls.push(format!("copy {src} {dest}"));
        if let Some((_, errno)) = self.failures.iter().find(|(key, _)| key == src) {
            return Err(OsError {
                errno: *errno,
                display: format!("[Errno {errno}] {}: '{src}'", strerror(*errno)),
            });
        }
        if self.files.iter().any(|(key, _)| key == src) {
            return Ok(());
        }
        Err(OsError::not_found(src))
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let call = format!("subp {}", argv.join(" "));
        self.calls.push(call.clone());
        match self.fail(&call) {
            None => Ok(()),
            Some(message) => Err(CmdError {
                command: ci_config::repr(&Value::Array(
                    argv.iter().map(|arg| Value::String(arg.clone())).collect(),
                )),
                exit_code: Some(1),
                stdout: String::new(),
                stderr: message,
            }),
        }
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
    use serde_json::json;

    fn object(value: serde_json::Value) -> Object {
        match value {
            serde_json::Value::Object(map) => map,
            _ => Object::new(),
        }
    }

    fn run_with(
        cfg: serde_json::Value,
        host: &mut Fixture,
    ) -> (Vec<String>, Result<(), String>) {
        let mut log = ci_log::Logger::capturing();
        let result = handle_with("cc_mcollective", &object(cfg), host, &mut log);
        (log.captured().to_vec(), result)
    }

    fn run(cfg: serde_json::Value) -> (Fixture, Vec<String>, Result<(), String>) {
        let mut host = Fixture::default();
        let (log, result) = run_with(cfg, &mut host);
        (host, log, result)
    }

    #[test]
    fn no_key_is_a_skip() {
        let (host, log, result) = run(json!({"other": 1}));
        assert!(result.is_ok());
        assert!(host.calls.is_empty());
        assert!(log[0].contains("no 'mcollective' key in configuration"));
    }

    #[test]
    fn an_empty_mapping_installs_and_restarts_only() {
        let (host, _, result) = run(json!({"mcollective": {}}));
        assert!(result.is_ok());
        assert_eq!(
            host.calls,
            [
                "install_packages ['mcollective']",
                "subp service mcollective restart",
            ]
        );
    }

    #[test]
    fn a_missing_server_cfg_starts_from_an_empty_config() {
        let (host, log, result) = run(json!({"mcollective": {"conf": {"a": "b"}}}));
        assert!(result.is_ok());
        assert_eq!(
            host.calls,
            [
                "install_packages ['mcollective']",
                "load_binary_file /etc/mcollective/server.cfg",
                "copy /etc/mcollective/server.cfg /etc/mcollective/server.cfg.old",
                "write_file /etc/mcollective/server.cfg mode=0644",
                "subp service mcollective restart",
            ]
        );
        assert_eq!(host.written[0].1, "a = b\n");
        assert!(log
            .iter()
            .any(|line| line.contains("starting with an empty config")));
    }

    #[test]
    fn an_existing_file_is_merged_and_copied_aside() {
        let mut host = Fixture {
            files: vec![(
                SERVER_CFG.to_owned(),
                "# lead\nidentity = old  # keep\n[plugin]\nx = 1\n".to_owned(),
            )],
            ..Fixture::default()
        };
        let (_, result) = run_with(
            json!({"mcollective": {"conf": {"identity": "new", "plugin": {"y": 2}}}}),
            &mut host,
        );
        assert!(result.is_ok());
        assert_eq!(
            host.calls[2],
            "copy /etc/mcollective/server.cfg /etc/mcollective/server.cfg.old"
        );
        assert_eq!(
            host.written[0].1,
            "# lead\nidentity = new# keep\n[plugin]\nx = 1\ny = 2\n"
        );
    }

    #[test]
    fn the_certificates_set_the_security_provider() {
        let (host, _, result) = run(json!({
            "mcollective": {"conf": {"public-cert": "PUB", "private-cert": "PRI"}}
        }));
        assert!(result.is_ok());
        assert_eq!(
            host.calls,
            [
                "install_packages ['mcollective']",
                "load_binary_file /etc/mcollective/server.cfg",
                "write_file /etc/mcollective/ssl/server-public.pem mode=0644",
                "write_file /etc/mcollective/ssl/server-private.pem mode=0600",
                "copy /etc/mcollective/server.cfg /etc/mcollective/server.cfg.old",
                "write_file /etc/mcollective/server.cfg mode=0644",
                "subp service mcollective restart",
            ]
        );
        assert_eq!(
            host.written[2].1,
            "plugin.ssl_server_public = /etc/mcollective/ssl/server-public.pem\n\
             securityprovider = ssl\n\
             plugin.ssl_server_private = /etc/mcollective/ssl/server-private.pem\n"
        );
    }

    #[test]
    fn a_non_string_scalar_is_stringified() {
        let (host, _, result) = run(json!({
            "mcollective": {"conf": {"a": 5, "b": true, "c": null, "d": ["x", "y"]}}
        }));
        assert!(result.is_ok());
        assert_eq!(
            host.written[0].1,
            "a = 5\nb = True\nc = None\nd = \"['x', 'y']\"\n"
        );
    }

    #[test]
    fn a_scalar_mcollective_is_subscripted_anyway() {
        let (host, _, result) = run(json!({"mcollective": "nothing here"}));
        assert!(result.is_ok());
        assert_eq!(
            host.calls,
            [
                "install_packages ['mcollective']",
                "subp service mcollective restart",
            ]
        );
        let (host, _, result) = run(json!({"mcollective": "conf"}));
        assert_eq!(
            result,
            Err("string indices must be integers, not 'str'".to_owned())
        );
        assert_eq!(host.calls, ["install_packages ['mcollective']"]);
        let (_, _, result) = run(json!({"mcollective": 5}));
        assert_eq!(
            result,
            Err("argument of type 'int' is not a container or iterable".to_owned())
        );
    }

    #[test]
    fn a_non_mapping_conf_dies_after_the_old_file_is_read() {
        let (host, _, result) = run(json!({"mcollective": {"conf": "hello"}}));
        assert_eq!(
            result,
            Err("'str' object has no attribute 'items'".to_owned())
        );
        assert_eq!(
            host.calls,
            [
                "install_packages ['mcollective']",
                "load_binary_file /etc/mcollective/server.cfg",
            ]
        );
    }

    #[test]
    fn a_server_cfg_that_does_not_parse_stops_the_module() {
        let mut host = Fixture {
            files: vec![(SERVER_CFG.to_owned(), "garbage\n".to_owned())],
            ..Fixture::default()
        };
        let (_, result) = run_with(json!({"mcollective": {"conf": {}}}), &mut host);
        assert_eq!(
            result,
            Err("Invalid line ('garbage') (matched as neither \
                 section nor keyword) at line 1."
                .to_owned())
        );
    }

    #[test]
    fn a_read_failure_that_is_not_enoent_is_reraised() {
        let mut host = Fixture {
            failures: vec![(SERVER_CFG.to_owned(), 13)],
            ..Fixture::default()
        };
        let (_, result) = run_with(json!({"mcollective": {"conf": {}}}), &mut host);
        assert_eq!(
            result,
            Err("[Errno 13] Permission denied: \
                 '/etc/mcollective/server.cfg'"
                .to_owned())
        );
    }

    #[test]
    fn a_non_string_certificate_raises_before_the_file_is_written() {
        let (host, _, result) =
            run(json!({"mcollective": {"conf": {"public-cert": 5}}}));
        assert_eq!(
            result,
            Err("'int' object has no attribute 'encode'".to_owned())
        );
        assert!(host.written.is_empty());
    }

    #[test]
    fn a_non_ascii_value_survives_the_merge_and_kills_the_write() {
        let (host, _, result) =
            run(json!({"mcollective": {"conf": {"identity": "caf\u{e9}"}}}));
        assert_eq!(
            result,
            Err("'ascii' codec can't encode character \
                 '\\xe9' in position 14: ordinal not in range(128)"
                .to_owned())
        );
        // The copy has already happened by then.
        assert_eq!(
            host.calls.last().unwrap(),
            "copy /etc/mcollective/server.cfg /etc/mcollective/server.cfg.old"
        );
    }

    #[test]
    fn a_restart_failure_is_the_module_failure() {
        let mut host = Fixture {
            errors: vec![(
                "subp service mcollective restart".to_owned(),
                "no such service".to_owned(),
            )],
            ..Fixture::default()
        };
        let (_, result) = run_with(json!({"mcollective": {}}), &mut host);
        assert!(result.unwrap_err().contains("no such service"));
    }
}
