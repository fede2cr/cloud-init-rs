//! `cc_keys_to_console` against a scripted machine, for the differential.
//! Paired with `tests/differential/cckeystoconsole.py`.
//!
//! Usage: `dump-cc-keys-to-console <case-json>`
//!        `dump-cc-keys-to-console --batch <cases-file>`
//!
//! The record is the log, the ordered list of things the module did, and what
//! it put on the console.

use ci_config::{Object, Value};
use ci_modules::cc::keys_to_console::{handle_with, Fixture};
use ci_modules::cc::rsyslog::CmdError;

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-keys-to-console: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-keys-to-console <case-json>");
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
        .unwrap_or("keys_to_console")
        .to_owned();
    let cfg = match case.get("cfg") {
        Some(Value::Object(map)) => map.clone(),
        _ => Object::new(),
    };
    let usr_lib_exec = case
        .get("usr_lib_exec")
        .and_then(Value::as_str)
        .unwrap_or("/usr/lib")
        .to_owned();
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&name, &cfg, &usr_lib_exec, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    out.insert("console".to_owned(), strings(&host.console));
    if let Err(error) = outcome {
        out.insert("error".to_owned(), Value::String(error));
    }
    Value::Object(out)
}

fn fixture(value: Option<&Value>) -> Fixture {
    let host = match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Object::new(),
    };
    Fixture {
        present: strings_of(host.get("present")),
        commands: match host.get("commands") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(key, spec)| (key.clone(), result_of(key, spec)))
                .collect(),
            _ => Vec::new(),
        },
        calls: Vec::new(),
        console: Vec::new(),
    }
}

/// A scripted command: a bare stdout string is a success, an object is a
/// failure with the three fields that vary.
fn result_of(key: &str, value: &Value) -> Result<String, CmdError> {
    match value {
        Value::Object(spec) => Err(CmdError {
            command: spec
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or(key)
                .to_owned(),
            exit_code: spec
                .get("exit_code")
                .and_then(Value::as_i64)
                .map_or(Some(1), |code| i32::try_from(code).ok()),
            stdout: spec
                .get("stdout")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            stderr: spec
                .get("stderr")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        other => Ok(other.as_str().unwrap_or_default().to_owned()),
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
