//! `cc_ssh_import_id` against a scripted machine, for the differential.
//! Paired with `tests/differential/ccsshimportid.py`.
//!
//! Usage: `dump-cc-ssh-import-id <case-json>`
//!        `dump-cc-ssh-import-id --batch <cases-file>`
//!
//! The case supplies the config, the module's run arguments, the normalized
//! user map, and which programs and accounts exist. `normalize_users_groups`
//! is *not* exercised here -- it has its own section -- so both sides are
//! handed the same already-normalized map.

use ci_config::{Object, Value};
use ci_modules::cc::rsyslog::CmdError;
use ci_modules::cc::ssh_import_id::{handle_with, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-ssh-import-id: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-ssh-import-id <case-json>");
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

    let cfg = object_of(case.get("cfg"));
    let users = object_of(case.get("users"));
    let args = case
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&cfg, &args, &users, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    if let Err(error) = outcome {
        out.insert("error".to_owned(), Value::String(error));
    }
    Value::Object(out)
}

fn object_of(value: Option<&Value>) -> Object {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Object::new(),
    }
}

fn fixture(value: Option<&Value>) -> Fixture {
    let host = object_of(value);
    Fixture {
        present: strings_of(host.get("present")),
        users: strings_of(host.get("users")),
        failures: match host.get("failures") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(call, spec)| (call.clone(), error_of(spec)))
                .collect(),
            _ => Vec::new(),
        },
        calls: Vec::new(),
    }
}

fn error_of(value: &Value) -> CmdError {
    let map = object_of(Some(value));
    CmdError {
        command: map
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        exit_code: map
            .get("exit_code")
            .and_then(Value::as_i64)
            .map_or(Some(1), |code| i32::try_from(code).ok()),
        // `capture=False`, so both streams print as the placeholder.
        stdout: "-".to_owned(),
        stderr: "-".to_owned(),
    }
}

fn strings_of(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn strings(lines: &[String]) -> Value {
    Value::Array(lines.iter().cloned().map(Value::String).collect())
}
