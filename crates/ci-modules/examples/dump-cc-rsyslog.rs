//! `cc_rsyslog` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccrsyslog.py`.
//!
//! Usage: `dump-cc-rsyslog <case-json>`
//!        `dump-cc-rsyslog --batch <cases-file>`
//!
//! The case supplies the config, the `system_info` block the distro would have
//! been built with, and every answer the machine could give: which programs
//! `which` finds, which calls fail and how, and which paths refuse to be
//! written. The record is the log, the ordered list of things the module did,
//! and what each file ended up holding.
//!
//! Batch mode takes one case per line and emits a `## <line>` marker before
//! each record.

use ci_config::{Object, Value};
use ci_modules::cc::rsyslog::{handle_with, CmdError, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-rsyslog: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-rsyslog <case-json>");
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
        .unwrap_or("rsyslog")
        .to_owned();
    let cfg = object_of(case.get("cfg"));
    let system_info = object_of(case.get("system_info"));
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&name, &cfg, &system_info, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    out.insert(
        "written".to_owned(),
        Value::Array(
            host.written
                .iter()
                .map(|(path, content)| {
                    Value::Array(vec![
                        Value::String(path.clone()),
                        Value::String(content.clone()),
                    ])
                })
                .collect(),
        ),
    );
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
        present: list_of(host.get("present")),
        failures: match host.get("failures") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(call, result)| (call.clone(), error_of(result)))
                .collect(),
            _ => Vec::new(),
        },
        unwritable: match host.get("unwritable") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(path, reason)| {
                    (path.clone(), reason.as_str().unwrap_or_default().to_owned())
                })
                .collect(),
            _ => Vec::new(),
        },
        calls: Vec::new(),
        written: Vec::new(),
    }
}

/// A scripted failure: the `Command:` line is filled in by the module, so the
/// case only supplies the three fields that vary.
fn error_of(value: &Value) -> CmdError {
    let map = match value {
        Value::Object(map) => map.clone(),
        _ => Object::new(),
    };
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
        stdout: map
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        stderr: map
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn list_of(value: Option<&Value>) -> Vec<String> {
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
