//! Port of `cc_keys_to_console.py`: run the helper that prints SSH host key
//! fingerprints, and put what it printed on the system console.
//!
//! The module does almost nothing itself -- the listing, the blacklisting and
//! the `ssh-keygen -l` calls all happen inside
//! `<usr_lib_exec>/cloud-init/write-ssh-key-fingerprints`, a POSIX shell
//! script the *Python* package ships. The port does not ship a copy of it
//! (the path would collide with that package, which is why everything else the
//! port installs lives under `/usr/libexec/cloud-init-rs`), so on a machine
//! with only the port installed this module takes upstream's own
//! "helper tool not found" branch. See docs/COMPAT.md.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::growpart::py_list;
use super::rsyslog::CmdError;
use super::Args;

const SOURCE: &str = "cc_keys_to_console.py";

/// `HELPER_TOOL_TPL`.
const HELPER_TOOL_TPL: &str = "%s/cloud-init/write-ssh-key-fingerprints";

/// What the module asks of the machine.
pub trait Host {
    /// `os.path.exists(path)`.
    fn exists(&mut self, path: &str) -> bool;

    /// `subp.subp(cmd)`, returning stdout.
    fn subp(&mut self, argv: &[String]) -> Result<String, CmdError>;

    /// `log_util.multi_log(text, stderr=False, console=True)`.
    fn multi_log(&mut self, text: &str);
}

/// `_get_helper_tool_path`.
#[must_use]
pub fn helper_tool_path(usr_lib_exec: &str) -> String {
    HELPER_TOOL_TPL.replacen("%s", usr_lib_exec, 1)
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// The `AttributeError` an `ssh:` that is not a mapping raises, and whatever
/// the helper failed with -- upstream logs a warning and re-raises.
pub fn handle_with(
    name: &str,
    cfg: &Object,
    usr_lib_exec: &str,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    // `cfg.get("ssh", {}).get("emit_keys_to_console", True)`: a present-but-
    // not-a-mapping `ssh:` has no `.get`, and that is not caught anywhere.
    let emit = match cfg.get("ssh") {
        None => Value::Bool(true),
        Some(Value::Object(ssh)) => ssh
            .get("emit_keys_to_console")
            .cloned()
            .unwrap_or(Value::Bool(true)),
        Some(other) => {
            return Err(format!(
                "'{}' object has no attribute 'get'",
                ci_config::type_name(other)
            ))
        }
    };
    if ci_config::option::is_false(&emit) {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, logging of SSH host keys \
                 disabled"
            ),
        );
        return Ok(());
    }

    let helper_path = helper_tool_path(usr_lib_exec);
    if !host.exists(&helper_path) {
        log.warning(
            SOURCE,
            &format!(
                "Unable to activate module {name}, helper tool not found at \
                 {helper_path}"
            ),
        );
        return Ok(());
    }

    // Everything from here is inside upstream's one `try`, the two joins
    // included -- so a blacklist with a number in it is reported as a failure
    // to write to the console.
    match run_helper(cfg, &helper_path, host) {
        Ok(()) => Ok(()),
        Err(error) => {
            log.warning(SOURCE, "Writing keys to the system console failed!");
            Err(error)
        }
    }
}

/// The body of `handle`'s `try`.
fn run_helper(
    cfg: &Object,
    helper_path: &str,
    host: &mut dyn Host,
) -> Result<(), String> {
    let argv = vec![
        helper_path.to_owned(),
        joined(cfg, "ssh_fp_console_blacklist")?,
        joined(cfg, "ssh_key_console_blacklist")?,
    ];
    let stdout = host.subp(&argv).map_err(|error| error.to_string())?;
    host.multi_log(&format!("{}\n", stdout.trim_matches(is_py_space)));
    Ok(())
}

/// `",".join(util.get_cfg_option_list(cfg, key, []))`.
///
/// `get_cfg_option_list` `str()`s a value that is not a list, but hands a list
/// straight back -- so a list with a number in it reaches `str.join`, which
/// only takes strings.
fn joined(cfg: &Object, key: &str) -> Result<String, String> {
    let items: Vec<String> = match cfg.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let Value::String(text) = item else {
                    return Err(format!(
                        "sequence item {index}: expected str instance, {} \
                         found",
                        ci_config::type_name(item)
                    ));
                };
                out.push(text.clone());
            }
            out
        }
        Some(other) => vec![super::py_str(other)],
    };
    Ok(items.join(","))
}

/// `str.strip()` with no argument.
fn is_py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}' | '\u{85}')
}

/// The registry entry point.
///
/// # Errors
/// As [`handle_with`].
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut host = Live {
        root: args.root.to_owned(),
    };
    let (name, cfg, usr_lib_exec) = (
        args.name.to_owned(),
        args.cfg.clone(),
        args.distro.usr_lib_exec.to_owned(),
    );
    handle_with(&name, &cfg, &usr_lib_exec, &mut host, &mut *args.logger)
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
}

impl Host for Live {
    fn exists(&mut self, path: &str) -> bool {
        self.root.join(path.trim_start_matches('/')).exists()
    }

    fn subp(&mut self, argv: &[String]) -> Result<String, CmdError> {
        let out = ci_sys::subp::Subp::new(argv)
            .run()
            .map_err(|error| CmdError {
                command: py_list(argv),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.code == Some(0) {
            return Ok(stdout);
        }
        Err(CmdError {
            command: py_list(argv),
            exit_code: out.code,
            stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn multi_log(&mut self, text: &str) {
        super::multi_log_console(text);
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    pub present: Vec<String>,
    /// Scripted results, keyed by the argv joined with spaces.
    pub commands: Vec<(String, Result<String, CmdError>)>,
    pub calls: Vec<String>,
    /// What reached the console.
    pub console: Vec<String>,
}

impl Host for Fixture {
    fn exists(&mut self, path: &str) -> bool {
        self.calls.push(format!("exists {path}"));
        self.present.iter().any(|name| name == path)
    }

    fn subp(&mut self, argv: &[String]) -> Result<String, CmdError> {
        let key = argv.join(" ");
        self.calls.push(format!("subp {key}"));
        self.commands
            .iter()
            .find(|(name, _)| name == &key)
            .map_or_else(
                || {
                    Err(CmdError {
                        command: py_list(argv),
                        exit_code: Some(127),
                        stdout: String::new(),
                        stderr: format!(
                            "{}: not found",
                            argv.first().cloned().unwrap_or_default()
                        ),
                    })
                },
                |(_, result)| result.clone(),
            )
    }

    fn multi_log(&mut self, text: &str) {
        self.calls.push("multi_log".to_owned());
        self.console.push(text.to_owned());
    }
}

#[cfg(test)]
#[expect(clippy::indexing_slicing, reason = "test assertions")]
mod tests {
    use super::*;

    const HELPER: &str = "/usr/lib/cloud-init/write-ssh-key-fingerprints";

    fn run(cfg: &Value, host: &mut Fixture) -> (Vec<String>, Result<(), String>) {
        let mut log = Logger::capturing();
        let cfg = match cfg {
            Value::Object(map) => map.clone(),
            _ => Object::new(),
        };
        let outcome = handle_with("keys_to_console", &cfg, "/usr/lib", host, &mut log);
        (log.captured().to_vec(), outcome)
    }

    #[test]
    fn what_the_helper_printed_goes_to_the_console_with_one_newline() {
        let mut host = Fixture {
            present: vec![HELPER.to_owned()],
            commands: vec![(
                format!("{HELPER}  "),
                Ok("-----BEGIN SSH HOST KEY KEYS-----\nssh-ed25519 AAA\n\n".to_owned()),
            )],
            ..Fixture::default()
        };
        let (_, outcome) = run(&serde_json::json!({}), &mut host);
        assert!(outcome.is_ok());
        assert_eq!(
            host.console[0],
            "-----BEGIN SSH HOST KEY KEYS-----\nssh-ed25519 AAA\n"
        );
    }

    #[test]
    fn the_two_blacklists_are_one_comma_joined_argument_each() {
        let mut host = Fixture {
            present: vec![HELPER.to_owned()],
            commands: vec![(
                format!("{HELPER} ssh-dss,ecdsa-sha2-nistp256 ssh-dss"),
                Ok(String::new()),
            )],
            ..Fixture::default()
        };
        let (_, outcome) = run(
            &serde_json::json!({
                "ssh_fp_console_blacklist": ["ssh-dss", "ecdsa-sha2-nistp256"],
                "ssh_key_console_blacklist": "ssh-dss",
            }),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host.calls.iter().any(|call| call
            == &format!("subp {HELPER} ssh-dss,ecdsa-sha2-nistp256 ssh-dss")));
    }

    #[test]
    fn emit_keys_to_console_false_stops_before_the_helper_is_looked_for() {
        let mut host = Fixture::default();
        let (log, outcome) = run(
            &serde_json::json!({"ssh": {"emit_keys_to_console": false}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(host.calls.is_empty());
        assert_eq!(
            log,
            ["cc_keys_to_console.py[DEBUG]: Skipping module named \
              keys_to_console, logging of SSH host keys disabled"]
        );
    }

    #[test]
    fn a_missing_helper_is_a_warning_and_nothing_else() {
        let mut host = Fixture::default();
        let (log, outcome) = run(&serde_json::json!({}), &mut host);
        assert!(outcome.is_ok());
        assert_eq!(host.calls, [format!("exists {HELPER}")]);
        assert_eq!(
            log,
            [format!(
                "cc_keys_to_console.py[WARNING]: Unable to activate module \
                 keys_to_console, helper tool not found at {HELPER}"
            )]
        );
    }

    #[test]
    fn a_helper_that_fails_is_logged_and_re_raised() {
        let mut host = Fixture {
            present: vec![HELPER.to_owned()],
            commands: vec![(
                format!("{HELPER}  "),
                Err(CmdError {
                    command: "x".to_owned(),
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "no keys\n".to_owned(),
                }),
            )],
            ..Fixture::default()
        };
        let (log, outcome) = run(&serde_json::json!({}), &mut host);
        assert!(outcome.is_err());
        assert!(host.console.is_empty());
        assert!(log.iter().any(|line| line
            == "cc_keys_to_console.py[WARNING]: Writing keys to the system \
                console failed!"));
    }

    #[test]
    fn an_ssh_key_that_is_not_a_mapping_has_no_get() {
        let mut host = Fixture::default();
        assert_eq!(
            run(&serde_json::json!({"ssh": "yes"}), &mut host).1,
            Err("'str' object has no attribute 'get'".to_owned())
        );
        assert_eq!(
            run(&serde_json::json!({"ssh": null}), &mut host).1,
            Err("'NoneType' object has no attribute 'get'".to_owned())
        );
    }
}
