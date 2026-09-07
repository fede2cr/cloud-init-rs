//! Port of `cloudinit/config/cc_snap.py`.
//!
//! Two independent halves behind one config key: importing assertions, which
//! is what lets a machine trust a store or a brand, and running arbitrary
//! `snap` commands. Neither is validated beyond its shape — `commands` is a
//! list of argument vectors *or* shell source, chosen per item, so this module
//! is one of the places tenant config becomes a root shell.
//!
//! Split into [`plan`] and [`run`] like the other command-executing modules:
//! everything upstream decides is decided by `plan`, and only `run` reaches
//! `snap(1)`.

use ci_config::{Object, Value};

use super::Args;

const SOURCE: &str = "cc_snap.py";

/// `SNAP_CMD`.
const SNAP_CMD: &str = "snap";

/// One thing the module does, in order.
///
/// The log lines are steps too. Upstream interleaves them with the writes and
/// the commands — a `Snap acking:` line per assertion before the file is
/// written, a warning between the last assertion and the first command — and
/// that order is part of what the differential compares, so it cannot live in
/// a side-channel logger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Debug(String),
    Warning(String),
    /// `util.wait_for_snap_seeded`. Guarded by `which("snap")` at run time,
    /// not here, because that is what upstream's callback checks.
    WaitSeeded,
    /// `util.write_file(assertions_file, combined.encode("utf-8"))`.
    WriteAssertions {
        path: String,
        contents: String,
    },
    /// `snap ack <file>`.
    Ack {
        path: String,
    },
    /// One entry of `snap: {commands: ...}` after `prepend_base_command`.
    Command(Command),
}

/// A `commands` entry. The two variants are not interchangeable: upstream
/// passes `shell=isinstance(command, str)`, so a string is handed to `/bin/sh`
/// and a list is executed directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A list entry. Held as `Value`s because upstream never stringifies them
    /// before `subp`, so a non-string element is `subp`'s problem, not ours.
    Argv(Vec<Value>),
    /// A string entry, run through a shell.
    Shell(String),
}

/// `handle`.
///
/// # Errors
/// A `commands` or `assertions` value of the wrong shape, or a command that
/// failed once every command has been attempted.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    // `os.path.join(cloud.paths.get_ipath_cur(), "snapd.assertions")`.
    let path = args.paths.instance_link().join("snapd.assertions");
    let path = path.to_string_lossy().into_owned();

    let (steps, failed) = plan(args.cfg, &path, args.name);
    // Upstream's side effects up to the raise are real, so the steps taken
    // before an error still run before it is reported.
    run(&steps, args)?;
    failed.map_or(Ok(()), Err)
}

/// Decide what the module would do, touching nothing.
///
/// `assertions_file` is `os.path.join(cloud.paths.get_ipath_cur(),
/// "snapd.assertions")`, resolved by the caller because it needs a datasource.
///
/// Returns the steps decided on *and* the exception that stopped it, because
/// upstream raises partway through: an `assertions` block can be written and
/// acked before a mis-shaped `commands` aborts the module.
pub fn plan(
    cfg: &Object,
    assertions_file: &str,
    name: &str,
) -> (Vec<Step>, Option<String>) {
    let cfgin = cfg.get("snap");
    if !cfgin.is_some_and(ci_config::option::py_truthy) {
        let skip =
            format!("Skipping module named {name}, no 'snap' key in configuration");
        return (vec![Step::Debug(skip)], None);
    }

    // `util.wait_for_snap_seeded` comes before the first `cfgin.get`, so it
    // happens even when the key turns out to hold something unusable.
    let mut steps = vec![Step::WaitSeeded];

    let Some(cfgin) = cfgin.and_then(Value::as_object) else {
        // `cfgin.get("assertions", [])` on a non-mapping.
        let kind = super::type_name(cfgin.unwrap_or(&Value::Null));
        return (
            steps,
            Some(format!("'{kind}' object has no attribute 'get'")),
        );
    };

    if let Err(error) =
        add_assertions(cfgin.get("assertions"), assertions_file, &mut steps)
    {
        return (steps, Some(error));
    }
    if let Err(error) = run_commands(cfgin.get("commands"), &mut steps) {
        return (steps, Some(error));
    }
    (steps, None)
}

/// `add_assertions`.
fn add_assertions(
    assertions: Option<&Value>,
    assertions_file: &str,
    steps: &mut Vec<Step>,
) -> Result<(), String> {
    let Some(assertions) = assertions else {
        return Ok(());
    };
    if !ci_config::option::py_truthy(assertions) {
        return Ok(());
    }
    steps.push(Step::Debug(
        "Importing user-provided snap assertions".to_owned(),
    ));

    // A mapping contributes its *values*, in insertion order -- unlike
    // `commands` just below, which sorts by key.
    let items: Vec<&Value> = match assertions {
        Value::Object(map) => map.values().collect(),
        Value::Array(list) => list.iter().collect(),
        other => {
            // `.format(assertions=assertions)` is `str()`, not `repr()`, so a
            // string lands in the message unquoted.
            let shown = super::py_str(other);
            return Err(format!(
                "assertion parameter was not a list or dict: {shown}"
            ));
        }
    };

    // `"\n".join(assertions)` refuses a non-string element before anything is
    // written, so one bad entry drops every assertion.
    let mut texts = Vec::with_capacity(items.len());
    for item in &items {
        let Some(text) = item.as_str() else {
            return Err(format!(
                "sequence item {}: expected str instance, {} found",
                texts.len(),
                super::type_name(item)
            ));
        };
        texts.push(text);
    }

    for text in &texts {
        // `asrt.split("\n")[0:2]`, printed as a Python list.
        let head: Vec<Value> = text
            .split('\n')
            .take(2)
            .map(|line| Value::from(line.to_owned()))
            .collect();
        let shown = ci_config::repr(&Value::Array(head));
        steps.push(Step::Debug(format!("Snap acking: {shown}")));
    }

    steps.push(Step::WriteAssertions {
        path: assertions_file.to_owned(),
        contents: texts.join("\n"),
    });
    steps.push(Step::Ack {
        path: assertions_file.to_owned(),
    });
    Ok(())
}

/// `run_commands` plus the `prepend_base_command` it calls.
fn run_commands(commands: Option<&Value>, steps: &mut Vec<Step>) -> Result<(), String> {
    let Some(commands) = commands else {
        return Ok(());
    };
    if !ci_config::option::py_truthy(commands) {
        return Ok(());
    }
    steps.push(Step::Debug(
        "Running user-provided snap commands".to_owned(),
    ));

    // `sorted(commands.items())` -- a mapping is ordered by key, which is the
    // documented way to control the order of a merged-in fragment.
    let items: Vec<&Value> = match commands {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            keys.into_iter().filter_map(|key| map.get(key)).collect()
        }
        Value::Array(list) => list.iter().collect(),
        other => {
            let shown = super::py_str(other);
            return Err(format!(
                "commands parameter was not a list or dict: {shown}"
            ));
        }
    };

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut fixed = Vec::new();
    for command in items {
        match command {
            Value::Array(argv) => {
                let mut argv = argv.clone();
                match argv.first() {
                    // An explicit null opts out of the prepend, for a command
                    // that has to set the environment up first.
                    Some(Value::Null) => {
                        argv.remove(0);
                    }
                    Some(first) if first.as_str() != Some(SNAP_CMD) => {
                        argv.insert(0, Value::from(SNAP_CMD));
                    }
                    Some(_) => {}
                    // `command[0]` on an empty list is an IndexError upstream,
                    // which aborts the module rather than skipping the entry.
                    None => return Err("list index out of range".to_owned()),
                }
                fixed.push(Command::Argv(argv));
            }
            Value::String(text) => {
                if !text.starts_with(&format!("{SNAP_CMD} ")) {
                    warnings.push(text.clone());
                }
                fixed.push(Command::Shell(text.clone()));
            }
            other => errors.push(super::py_str(other)),
        }
    }

    if !warnings.is_empty() {
        let joined = warnings.join("\n");
        steps.push(Step::Warning(format!(
            "Non-{SNAP_CMD} commands in {SNAP_CMD} config:\n{joined}"
        )));
    }
    if !errors.is_empty() {
        let joined = errors.join("\n");
        return Err(format!(
            "Invalid {SNAP_CMD} config. These commands are not a string or list:\n{joined}"
        ));
    }

    steps.extend(fixed.into_iter().map(Step::Command));
    Ok(())
}

/// Carry out a plan. Nothing here runs unless the module's root is `/`, for
/// the same reason as every other command-executing module: a fixture root
/// must not be able to install snaps on the machine running the tests.
fn run(steps: &[Step], args: &mut Args<'_>) -> Result<(), String> {
    let live = args.root == std::path::Path::new("/");
    let mut failures = Vec::new();

    for step in steps {
        match step {
            Step::Debug(message) => args.debug(SOURCE, message),
            Step::Warning(message) => args.warning(SOURCE, message),
            Step::WaitSeeded => {
                if live {
                    if ci_sys::subp::which(SNAP_CMD).is_none() {
                        args.debug(
                            SOURCE,
                            "Skipping snap wait, no snap command present",
                        );
                        continue;
                    }
                    ci_sys::subp::Subp::new([
                        SNAP_CMD,
                        "wait",
                        "system",
                        "seed.loaded",
                    ])
                    .check()
                    .map_err(|error| error.to_string())?;
                }
            }
            Step::WriteAssertions { path, contents } => {
                super::write_file(
                    &super::rooted(args.root, path),
                    contents.as_bytes(),
                )?;
            }
            Step::Ack { path } => {
                if live {
                    ci_sys::subp::Subp::new([SNAP_CMD, "ack", path.as_str()])
                        .check()
                        .map_err(|error| error.to_string())?;
                }
            }
            // Every command is attempted; the failures are reported together
            // at the end, so one bad command does not hide the next.
            Step::Command(command) => {
                if live {
                    if let Err(error) = execute(command) {
                        failures.push(error);
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        return Ok(());
    }
    let shown = ci_config::repr(&Value::Array(
        failures.into_iter().map(Value::from).collect(),
    ));
    Err(format!("Failures running snap commands:\n{shown}"))
}

fn execute(command: &Command) -> Result<(), String> {
    match command {
        Command::Argv(argv) => {
            let argv: Vec<String> = argv.iter().map(super::py_str).collect();
            ci_sys::subp::Subp::new(argv.iter().map(String::as_str))
                .check()
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        Command::Shell(source) => {
            ci_sys::subp::Subp::new(["/bin/sh", "-c", source.as_str()])
                .check()
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod tests {
    use super::*;

    const FILE: &str = "/var/lib/cloud/instance/snapd.assertions";

    fn plan_for(json: &str) -> (Vec<Step>, Option<String>) {
        let Value::Object(cfg) = serde_json::from_str(json).unwrap() else {
            panic!("test config must be an object")
        };
        plan(&cfg, FILE, "snap")
    }

    fn commands(steps: &[Step]) -> Vec<&Command> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Command(command) => Some(command),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn no_snap_key_does_not_even_wait_for_seeding() {
        let (steps, failed) = plan_for(r#"{"other": 1}"#);
        assert!(failed.is_none());
        assert!(matches!(steps.as_slice(), [Step::Debug(_)]));
    }

    #[test]
    fn a_mapping_of_assertions_keeps_insertion_order_but_commands_sort() {
        // The two halves of one module disagree about what a mapping means,
        // which is worth pinning because nothing in the schema says so.
        let (steps, failed) = plan_for(
            r#"{"snap": {"assertions": {"b": "bee", "a": "ay"},
                        "commands": {"z": "snap z", "a": "snap a"}}}"#,
        );
        assert!(failed.is_none());
        let written = steps.iter().find_map(|step| match step {
            Step::WriteAssertions { contents, .. } => Some(contents.as_str()),
            _ => None,
        });
        assert_eq!(written, Some("bee\nay"));
        assert_eq!(
            commands(&steps),
            vec![
                &Command::Shell("snap a".to_owned()),
                &Command::Shell("snap z".to_owned()),
            ]
        );
    }

    #[test]
    fn a_leading_null_opts_out_of_the_prepend_and_anything_else_gets_it() {
        let (steps, failed) = plan_for(
            r#"{"snap": {"commands": [["snap", "install", "a"],
                                     ["install", "b"],
                                     [null, "env", "X=1"]]}}"#,
        );
        assert!(failed.is_none());
        let argv = |words: &[&str]| {
            Command::Argv(words.iter().map(|w| Value::from((*w).to_owned())).collect())
        };
        assert_eq!(
            commands(&steps),
            vec![
                &argv(&["snap", "install", "a"]),
                &argv(&["snap", "install", "b"]),
                &argv(&["env", "X=1"]),
            ]
        );
    }

    #[test]
    fn a_string_command_is_a_shell_command_and_warns_unless_it_starts_with_snap() {
        let (steps, failed) =
            plan_for(r#"{"snap": {"commands": ["echo hi", "snap install x"]}}"#);
        assert!(failed.is_none());
        let warning = steps.iter().find_map(|step| match step {
            Step::Warning(message) => Some(message.as_str()),
            _ => None,
        });
        assert_eq!(warning, Some("Non-snap commands in snap config:\necho hi"));
        // Warned about, but still run.
        assert_eq!(commands(&steps).len(), 2);
    }

    #[test]
    fn an_empty_command_list_aborts_after_the_assertions_are_already_written() {
        // The IndexError lands partway through, so this is not "nothing
        // happened" -- the assertions file is on disk and acked by then.
        let (steps, failed) =
            plan_for(r#"{"snap": {"assertions": ["a"], "commands": [[]]}}"#);
        assert_eq!(failed.as_deref(), Some("list index out of range"));
        assert!(steps.iter().any(|s| matches!(s, Step::Ack { .. })));
        assert!(commands(&steps).is_empty());
    }

    #[test]
    fn a_non_string_assertion_drops_every_assertion() {
        let (steps, failed) = plan_for(r#"{"snap": {"assertions": ["ok", 7]}}"#);
        assert_eq!(
            failed.as_deref(),
            Some("sequence item 1: expected str instance, int found")
        );
        // The join fails before the per-assertion debug lines, so not even
        // the good one is announced.
        assert!(!steps
            .iter()
            .any(|s| matches!(s, Step::WriteAssertions { .. })));
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, Step::Debug(m) if m.starts_with("Snap acking")))
                .count(),
            0
        );
    }

    #[test]
    fn a_snap_key_that_is_not_a_mapping_still_waits_for_seeding_first() {
        let (steps, failed) = plan_for(r#"{"snap": "install-everything"}"#);
        assert_eq!(
            failed.as_deref(),
            Some("'str' object has no attribute 'get'")
        );
        assert_eq!(steps.as_slice(), [Step::WaitSeeded]);
    }
}
