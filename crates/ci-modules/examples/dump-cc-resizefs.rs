//! `cc_resizefs` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccresizefs.py`.
//!
//! Usage: `dump-cc-resizefs <case-json>`
//!        `dump-cc-resizefs --batch <cases-file>`
//!
//! The case supplies the config, the module's run arguments, and every answer
//! the module could get from a machine -- command output and exit codes, which
//! paths exist, what `stat` says, what is mounted where and with which
//! options. The record is the log and the ordered list of questions the module
//! asked.
//!
//! Nothing here runs a command or touches a filesystem, on either side: the
//! Python half stubs `subp.subp` and the `os` probes onto the same script.
//!
//! Batch mode takes one case per line and emits a `## <line>` marker before
//! each record.

use ci_config::{Object, Value};
use ci_modules::cc::resizefs::{handle_with, CommandResult, Fixture, Mount};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-resizefs: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-resizefs <case-json>");
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

    let name = case
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("resizefs")
        .to_owned();
    let cfg = match case.get("cfg") {
        Some(Value::Object(cfg)) => cfg.clone(),
        _ => Object::new(),
    };
    let args = case
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&name, &cfg, &args, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    if let Err(error) = outcome {
        out.insert("error".to_owned(), Value::String(error));
    }
    Value::Object(out)
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
        exists: list("exists"),
        dirs: list("dirs"),
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
                        .unwrap_or(0)
                        .try_into()
                        .unwrap_or(0),
                    stdout: text("stdout"),
                    stderr: text("stderr"),
                },
            )
        })
        .collect()
}

/// `mounts`, given as `[device, fstype, mount-point, options]`.
fn mounts_of(entries: &[(String, Value)]) -> Vec<(String, Mount)> {
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
                Mount {
                    devpth: at(0),
                    fs_type: at(1),
                    mount_point: at(2),
                    opts: at(3),
                },
            ))
        })
        .collect()
}

/// `devs`, keyed by the `blkid` criteria.
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
