//! Port of `cc_ssh_import_id.py`: run `ssh-import-id` for the users that ask
//! for it.
//!
//! The module runs one command per user and cannot be split into "decide, then
//! act": whether it runs at all depends on `which(ssh-import-id)`, which
//! privilege tool it uses depends on `which(sudo)` and then `which(doas)`, and
//! a user that does not exist ends the whole module rather than that user. So
//! the machine goes behind [`Host`], with [`Fixture`] answering from a script
//! and recording what was asked.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::growpart::{logexc, py_list};
use super::rsyslog::CmdError;
use super::Args;

const SOURCE: &str = "cc_ssh_import_id.py";

/// `SSH_IMPORT_ID_BINARY`.
const SSH_IMPORT_ID_BINARY: &str = "ssh-import-id";

/// What the module asks of the machine.
pub trait Host {
    /// `subp.which(program)`.
    fn which(&mut self, program: &str) -> bool;

    /// `pwd.getpwnam(user)`, whose `KeyError` upstream re-raises unchanged.
    fn getpwnam(&mut self, user: &str) -> bool;

    /// `subp.subp(cmd, capture=False)`.
    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError>;
}

/// `is_key_in_nested_dict`.
///
/// Walks mappings, and lists only for the mappings directly inside them --
/// upstream's own comment says a dict inside a list of lists is missed.
#[must_use]
pub fn is_key_in_nested_dict(config: &Object, search_key: &str) -> bool {
    for (key, value) in config {
        if key == search_key {
            return true;
        }
        match value {
            Value::Object(map) => {
                if is_key_in_nested_dict(map, search_key) {
                    return true;
                }
            }
            Value::Array(items) => {
                for item in items {
                    if let Value::Object(map) = item {
                        if is_key_in_nested_dict(map, search_key) {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// `import_ssh_ids`.
///
/// `user` is the value as configured rather than its text: upstream tests its
/// truthiness, interpolates `str()` of it and, when `getpwnam` fails, raises a
/// `KeyError` whose message is `repr()` of it -- three different renderings of
/// the same object.
///
/// # Errors
/// The `KeyError` a missing user raises, and the `ProcessExecutionError` the
/// command failing raises -- both after being logged.
pub fn import_ssh_ids(
    host: &mut dyn Host,
    ids: &[String],
    user: &Value,
    log: &mut Logger,
) -> Result<(), String> {
    let name = super::py_str(user);
    if !ci_config::option::py_truthy(user) || ids.is_empty() {
        log.debug(
            SOURCE,
            &format!("empty user({name}) or ids({}). not importing", py_list(ids)),
        );
        return Ok(());
    }

    if !host.getpwnam(&name) {
        return Err(ci_config::repr(user));
    }

    let argv = if host.which("sudo") {
        let mut argv = vec![
            "sudo".to_owned(),
            "--preserve-env=https_proxy".to_owned(),
            "-Hu".to_owned(),
            name.clone(),
            SSH_IMPORT_ID_BINARY.to_owned(),
        ];
        argv.extend(ids.iter().cloned());
        argv
    } else if host.which("doas") {
        let mut argv = vec![
            "doas".to_owned(),
            "-u".to_owned(),
            name.clone(),
            SSH_IMPORT_ID_BINARY.to_owned(),
        ];
        argv.extend(ids.iter().cloned());
        argv
    } else {
        log.error(
            SOURCE,
            "Neither sudo nor doas available! Unable to import SSH ids.",
        );
        return Ok(());
    };

    log.debug(SOURCE, &format!("Importing SSH ids for user {name}."));
    match host.subp(&argv) {
        Ok(()) => Ok(()),
        Err(error) => {
            logexc(
                log,
                &format!("Failed to run command to import {name} SSH ids"),
            );
            Err(error.to_string())
        }
    }
}

/// `handle`, against a scripted machine.
///
/// # Errors
/// The first exception any user's import raised, re-raised after every user
/// has been tried.
pub fn handle_with(
    cfg: &Object,
    args: &Value,
    users: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    if !is_key_in_nested_dict(cfg, "ssh_import_id") {
        log.debug(
            SOURCE,
            "Skipping module named ssh_import_id, no 'ssh_import_id' \
             directives found.",
        );
        return Ok(());
    }
    if !host.which(SSH_IMPORT_ID_BINARY) {
        log.warning(
            SOURCE,
            "ssh-import-id is not installed, but module ssh_import_id is \
             configured. Skipping module.",
        );
        return Ok(());
    }

    // `if args:` -- the run arguments name one user and, after it, the ids.
    if let Value::Array(items) = args {
        if !items.is_empty() {
            let user = items.first().cloned().unwrap_or(Value::Null);
            let ids: Vec<String> = items.iter().skip(1).map(super::py_str).collect();
            return import_ssh_ids(host, &ids, &user, log);
        }
    }

    let mut errors: Vec<String> = Vec::new();
    for (user, user_cfg) in users {
        // `user_cfg["default"]`, not `.get`: a map without the key is a
        // `KeyError` that ends the module. `normalize_users_groups` always
        // sets it, so upstream never reaches this with its own input.
        let default = match user_cfg {
            Value::Object(map) => match map.get("default") {
                Some(value) => ci_config::option::py_truthy(value),
                None => return Err("'default'".to_owned()),
            },
            other => {
                return Err(match other {
                    Value::String(_) => {
                        "string indices must be integers, not 'str'".to_owned()
                    }
                    other => format!(
                        "'{}' object is not subscriptable",
                        ci_config::type_name(other)
                    ),
                })
            }
        };
        let raw = if default {
            option_list(cfg, "ssh_import_id")
        } else if let Some(value) = user_cfg.get("ssh_import_id") {
            value.clone()
        } else {
            log.debug(
                SOURCE,
                &format!("User {user} is not configured for ssh_import_id"),
            );
            continue;
        };

        // `util.uniq_merge(import_ids)` then `[str(i) for i in ...]`; both
        // are inside one `try` whose `except` only says the user is not
        // configured correctly.
        let Ok(merged) = ci_distro::ug::uniq_merge(&[&raw]) else {
            log.debug(
                SOURCE,
                &format!("User {user} is not correctly configured for ssh_import_id"),
            );
            continue;
        };
        let import_ids: Vec<String> = merged.iter().map(super::py_str).collect();
        if import_ids.is_empty() {
            continue;
        }

        if let Err(error) =
            import_ssh_ids(host, &import_ids, &Value::String(user.clone()), log)
        {
            logexc(
                log,
                &format!("ssh-import-id failed for: {user} {}", py_list(&import_ids)),
            );
            errors.push(error);
        }
    }

    // `raise elist[0]` -- the *first* failure, not the last.
    errors.first().map_or(Ok(()), |error| Err(error.clone()))
}

/// `util.get_cfg_option_list(cfg, "ssh_import_id", [])`.
fn option_list(cfg: &Object, key: &str) -> Value {
    match cfg.get(key) {
        None | Some(Value::Null) => Value::Array(Vec::new()),
        Some(Value::Array(items)) => Value::Array(items.clone()),
        Some(Value::String(text)) => Value::Array(vec![Value::String(text.clone())]),
        Some(other) => Value::Array(vec![Value::String(super::py_str(other))]),
    }
}

/// The registry entry point.
///
/// # Errors
/// As [`handle_with`].
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let default_user = args.system_info.get("default_user").cloned();
    let normalized = ci_distro::ug::normalize_users_groups(
        args.cfg,
        default_user.as_ref(),
        args.logger,
    )?;
    let mut host = Live {
        root: args.root.to_owned(),
    };
    let (cfg, extra) = (args.cfg.clone(), args.args.clone());
    handle_with(
        &cfg,
        &extra,
        &normalized.users,
        &mut host,
        &mut *args.logger,
    )
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
}

impl Host for Live {
    fn which(&mut self, program: &str) -> bool {
        ci_sys::subp::which(program).is_some()
    }

    fn getpwnam(&mut self, user: &str) -> bool {
        ci_sys::ids::passwd_entry(&self.root, user).is_some()
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let out = ci_sys::subp::Subp::new(argv)
            .run()
            .map_err(|error| CmdError {
                command: py_list(argv),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        if out.code == Some(0) {
            return Ok(());
        }
        Err(CmdError {
            command: py_list(argv),
            exit_code: out.code,
            // `capture=False`, so neither stream was collected and both print
            // as the `-` placeholder.
            stdout: "-".to_owned(),
            stderr: "-".to_owned(),
        })
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    pub present: Vec<String>,
    pub users: Vec<String>,
    /// Failures, keyed by the argv joined with spaces.
    pub failures: Vec<(String, CmdError)>,
    pub calls: Vec<String>,
}

impl Host for Fixture {
    fn which(&mut self, program: &str) -> bool {
        self.calls.push(format!("which {program}"));
        self.present.iter().any(|name| name == program)
    }

    fn getpwnam(&mut self, user: &str) -> bool {
        self.calls.push(format!("getpwnam {user}"));
        self.users.iter().any(|name| name == user)
    }

    fn subp(&mut self, argv: &[String]) -> Result<(), CmdError> {
        let key = argv.join(" ");
        self.calls.push(format!("subp {key}"));
        self.failures.iter().find(|(name, _)| name == &key).map_or(
            Ok(()),
            |(_, error)| {
                let mut error = error.clone();
                if error.command.is_empty() {
                    error.command = py_list(argv);
                }
                Err(error)
            },
        )
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn users(spec: &Value) -> Object {
        match spec {
            Value::Object(map) => map.clone(),
            _ => Object::new(),
        }
    }

    fn run(
        cfg: &Value,
        user_map: &Value,
        host: &mut Fixture,
    ) -> (Vec<String>, Result<(), String>) {
        let mut log = Logger::capturing();
        let cfg = users(cfg);
        let outcome = handle_with(
            &cfg,
            &Value::Array(Vec::new()),
            &users(user_map),
            host,
            &mut log,
        );
        (log.captured().to_vec(), outcome)
    }

    #[test]
    fn the_default_user_takes_the_top_level_list() {
        let mut host = Fixture {
            present: vec!["ssh-import-id".to_owned(), "sudo".to_owned()],
            users: vec!["ubuntu".to_owned()],
            ..Fixture::default()
        };
        let (_, outcome) = run(
            &serde_json::json!({"ssh_import_id": ["lp:alice", "gh:bob"]}),
            &serde_json::json!({"ubuntu": {"default": true}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert_eq!(
            host.calls,
            [
                "which ssh-import-id",
                "getpwnam ubuntu",
                "which sudo",
                "subp sudo --preserve-env=https_proxy -Hu ubuntu \
                 ssh-import-id lp:alice gh:bob",
            ]
        );
    }

    #[test]
    fn doas_is_used_only_when_sudo_is_missing() {
        let mut host = Fixture {
            present: vec!["ssh-import-id".to_owned(), "doas".to_owned()],
            users: vec!["ubuntu".to_owned()],
            ..Fixture::default()
        };
        let _ = run(
            &serde_json::json!({"ssh_import_id": ["lp:alice"]}),
            &serde_json::json!({"ubuntu": {"default": true}}),
            &mut host,
        );
        assert_eq!(
            host.calls.last().unwrap(),
            "subp doas -u ubuntu ssh-import-id lp:alice"
        );
    }

    #[test]
    fn with_neither_sudo_nor_doas_it_says_so_and_runs_nothing() {
        let mut host = Fixture {
            present: vec!["ssh-import-id".to_owned()],
            users: vec!["ubuntu".to_owned()],
            ..Fixture::default()
        };
        let (log, outcome) = run(
            &serde_json::json!({"ssh_import_id": ["lp:alice"]}),
            &serde_json::json!({"ubuntu": {"default": true}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert!(!host.calls.iter().any(|call| call.starts_with("subp")));
        assert!(log.iter().any(|line| line
            == "cc_ssh_import_id.py[ERROR]: Neither sudo nor doas available! \
                Unable to import SSH ids."));
    }

    #[test]
    fn a_non_default_user_reads_its_own_key() {
        let mut host = Fixture {
            present: vec!["ssh-import-id".to_owned(), "sudo".to_owned()],
            users: vec!["alice".to_owned()],
            ..Fixture::default()
        };
        let (log, _) = run(
            &serde_json::json!({"users": [{"name": "alice",
                                           "ssh_import_id": ["lp:alice"]}]}),
            &serde_json::json!({
                "alice": {"default": false, "ssh_import_id": ["lp:alice"]},
                "bob": {"default": false},
            }),
            &mut host,
        );
        assert!(host.calls.iter().any(|call| call
            == "subp sudo --preserve-env=https_proxy -Hu alice ssh-import-id \
                lp:alice"));
        assert!(log.iter().any(|line| line
            == "cc_ssh_import_id.py[DEBUG]: User bob is not configured for \
                ssh_import_id"));
    }

    #[test]
    fn a_missing_binary_stops_the_module_with_a_warning() {
        let mut host = Fixture::default();
        let (log, outcome) = run(
            &serde_json::json!({"ssh_import_id": ["lp:alice"]}),
            &serde_json::json!({"ubuntu": {"default": true}}),
            &mut host,
        );
        assert!(outcome.is_ok());
        assert_eq!(host.calls, ["which ssh-import-id"]);
        assert!(log.iter().any(|line| line
            == "cc_ssh_import_id.py[WARNING]: ssh-import-id is not installed, \
                but module ssh_import_id is configured. Skipping module."));
    }

    #[test]
    fn a_user_that_does_not_exist_ends_the_module_with_a_key_error() {
        let mut host = Fixture {
            present: vec!["ssh-import-id".to_owned(), "sudo".to_owned()],
            ..Fixture::default()
        };
        let (_, outcome) = run(
            &serde_json::json!({"ssh_import_id": ["lp:alice"]}),
            &serde_json::json!({"ghost": {"default": true}}),
            &mut host,
        );
        assert_eq!(outcome, Err("'ghost'".to_owned()));
    }

    #[test]
    fn the_key_is_looked_for_at_every_depth_a_config_can_nest_it() {
        let nested = serde_json::json!({
            "users": [{"name": "alice", "ssh_import_id": ["lp:alice"]}],
        });
        assert!(is_key_in_nested_dict(
            nested.as_object().unwrap(),
            "ssh_import_id"
        ));
        let in_a_list_of_lists = serde_json::json!({
            "users": [[{"ssh_import_id": ["lp:alice"]}]],
        });
        // Upstream's own comment: a dict inside a list of lists is missed.
        assert!(!is_key_in_nested_dict(
            in_a_list_of_lists.as_object().unwrap(),
            "ssh_import_id"
        ));
    }
}
