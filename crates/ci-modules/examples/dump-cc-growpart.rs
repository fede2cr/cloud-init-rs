//! `cc_growpart` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccgrowpart.py`.
//!
//! Usage: `dump-cc-growpart <case-json>`
//!        `dump-cc-growpart --batch <cases-file>`
//!
//! The case supplies the config and every answer the module could get from a
//! machine -- command output and exit codes, which paths exist, what `stat`
//! and `lseek` say, what is mounted where. The record is the log, the ordered
//! list of questions the module asked, and the `(device, action, message)`
//! triples it ended with.
//!
//! Nothing here runs a command or touches a device, on either side: the
//! Python half stubs `subp.subp` and the `os` probes onto the same script.
//!
//! Batch mode takes one case per line and emits a `## <line>` marker before
//! each record.

use ci_config::{Object, Value};
use ci_modules::cc::growpart::{handle_with, CommandResult, Fixture, Mounted, Outcome};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-growpart: cannot read {:?}", arg(2));
            return std::process::ExitCode::from(2);
        };
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            println!("## {line}");
            println!("{}", ci_core::jsonfmt::dumps_indent(&one(line), 1));
        }
        return std::process::ExitCode::SUCCESS;
    }

    if argv.len() < 2 {
        eprintln!("usage: dump-cc-growpart <case-json>");
        return std::process::ExitCode::from(2);
    }
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(arg(1)), 1));
    std::process::ExitCode::SUCCESS
}

fn one(text: &str) -> Value {
    let mut out = Object::new();
    let Ok(Value::Object(case)) = serde_json::from_str::<Value>(text) else {
        out.insert(
            "error".to_owned(),
            Value::String("<case-json> must be an object".to_owned()),
        );
        return Value::Object(out);
    };

    let cfg = match case.get("cfg") {
        Some(Value::Object(cfg)) => cfg.clone(),
        _ => Object::new(),
    };
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&cfg, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    match outcome {
        Ok(outcomes) => {
            out.insert(
                "resized".to_owned(),
                Value::Array(outcomes.iter().map(triple).collect()),
            );
        }
        Err(error) => {
            out.insert("error".to_owned(), Value::String(error));
        }
    }
    Value::Object(out)
}

/// One `(entry-in-devices, action, message)` row.
fn triple(outcome: &Outcome) -> Value {
    Value::Array(vec![
        outcome.devent.clone(),
        Value::String(outcome.action.name().to_owned()),
        Value::String(outcome.message.clone()),
    ])
}

fn strings(items: &[String]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| Value::String(item.clone()))
            .collect(),
    )
}

/// The `host` half of a case: every answer, keyed the way the module asks.
fn fixture(spec: Option<&Value>) -> Fixture {
    let empty = Object::new();
    let spec = spec.and_then(Value::as_object).unwrap_or(&empty);
    let list = |key: &str| -> Vec<String> {
        spec.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let map = |key: &str| -> Vec<(String, Value)> {
        spec.get(key)
            .and_then(Value::as_object)
            .map(|items| {
                items
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };

    Fixture {
        commands: commands_of(&map("commands")),
        which: list("which"),
        exists: list("exists"),
        files: list("files"),
        realpath: map("realpath")
            .into_iter()
            .map(|(name, value)| (name, value.as_str().unwrap_or_default().to_owned()))
            .collect(),
        // Given in octal, because that is how a mode is read.
        stat: map("stat")
            .into_iter()
            .filter_map(|(name, value)| {
                let text = value.as_str()?;
                let mode =
                    u32::from_str_radix(text.trim_start_matches("0o"), 8).ok()?;
                Some((name, mode))
            })
            .collect(),
        sizes: map("sizes")
            .into_iter()
            .map(|(name, value)| {
                // A bare number is a device whose size never changes; a null
                // in the list is a read that finds nothing.
                let values = match value {
                    Value::Array(items) => items.iter().map(Value::as_u64).collect(),
                    other => vec![other.as_u64()],
                };
                (name, values)
            })
            .collect(),
        text: map("text")
            .into_iter()
            .map(|(name, value)| (name, value.as_str().unwrap_or_default().to_owned()))
            .collect(),
        mounts: mounts_of(&map("mounts")),
        container: spec
            .get("container")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cmdline: spec
            .get("cmdline")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        devs: devs_of(&map("devs")),
        tmp_exec: spec
            .get("tmp_exec")
            .and_then(Value::as_str)
            .unwrap_or("/var/tmp/cloud-init")
            .to_owned(),
        tmpdir: spec
            .get("tmpdir")
            .and_then(Value::as_str)
            .unwrap_or("/var/tmp/cloud-init/tmpfixture")
            .to_owned(),
        keydata: spec
            .get("keydata")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        calls: Vec::new(),
    }
}

/// `commands`, keyed by the argv joined with spaces.
fn commands_of(entries: &[(String, Value)]) -> Vec<(String, CommandResult)> {
    entries
        .iter()
        .map(|(name, value)| {
            let text = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            (
                name.clone(),
                CommandResult {
                    exit_code: value
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .and_then(|code| i32::try_from(code).ok())
                        .unwrap_or(0),
                    stdout: text("stdout"),
                    stderr: text("stderr"),
                },
            )
        })
        .collect()
}

/// `mounts`: path to `util.get_mount_info`'s first three fields.
fn mounts_of(entries: &[(String, Value)]) -> Vec<(String, Mounted)> {
    entries
        .iter()
        .filter_map(|(name, value)| {
            let fields = value.as_array()?;
            let at = |index: usize| {
                fields
                    .get(index)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Some((
                name.clone(),
                Mounted {
                    devpth: at(0),
                    fs_type: at(1),
                    mount_point: at(2),
                },
            ))
        })
        .collect()
}

/// `devs`: `util.find_devs_with` criteria to what it finds.
fn devs_of(entries: &[(String, Value)]) -> Vec<(String, Vec<String>)> {
    entries
        .iter()
        .map(|(name, value)| {
            let found = value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            (name.clone(), found)
        })
        .collect()
}
