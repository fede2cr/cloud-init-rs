//! `cc_disk_setup` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccdisksetup.py`.
//!
//! Usage: `dump-cc-disk-setup <case-json>`
//!        `dump-cc-disk-setup --batch <cases-file>`
//!
//! The case supplies the config and every answer the module could get from a
//! machine: command output and exit codes, which paths exist, which are block
//! devices, what `realpath` resolves to and whether the end-of-disk wipe
//! succeeds. The record is the log and the ordered list of questions the
//! module asked -- which for this module *is* the outcome, since everything it
//! does it does by running a command.
//!
//! Nothing here runs a command or touches a device, on either side.
//!
//! Batch mode takes one case per line and emits a `## <line>` marker before
//! each record.

use ci_config::{Object, Value};
use ci_modules::cc::disk_setup::{handle_with, Fixture};
use ci_modules::cc::growpart::CommandResult;

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-disk-setup: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-disk-setup <case-json>");
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
    let strings_of = |key: &str| -> Vec<(String, String)> {
        map(key)
            .into_iter()
            .map(|(name, value)| (name, value.as_str().unwrap_or_default().to_owned()))
            .collect()
    };

    Fixture {
        commands: commands_of(&map("commands")),
        shell: commands_of(&map("shell")),
        which: strings_of("which"),
        exists: list("exists"),
        block: list("block"),
        realpath: strings_of("realpath"),
        wipe: strings_of("wipe"),
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
