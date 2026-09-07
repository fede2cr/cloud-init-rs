//! `cc_ansible` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccansible.py`.
//!
//! Usage: `dump-cc-ansible <case-json>`
//!        `dump-cc-ansible --batch <cases-file>`
//!
//! The case supplies the config and every answer the machine could give: what
//! each command prints, which calls fail, which programs `which` finds,
//! whether `import pip` works and whether the stdlib is marked externally
//! managed. The record is the log, the ordered list of things the module did,
//! and what it wrote to stdout.

use ci_config::{Object, Value};
use ci_modules::cc::ansible::{handle_with, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-ansible: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-ansible <case-json>");
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
    let python = case
        .get("python")
        .and_then(Value::as_str)
        .unwrap_or("/usr/bin/python3")
        .to_owned();
    let home = case
        .get("home")
        .and_then(Value::as_str)
        .unwrap_or("/root")
        .to_owned();
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&cfg, &python, &home, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    out.insert("console".to_owned(), strings(&host.console));
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
        failures: table(host.get("failures")),
        stdout: table(host.get("stdout")),
        present: list_of(host.get("present")),
        pip: host.get("pip").and_then(Value::as_bool).unwrap_or(true),
        managed: host
            .get("managed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        calls: Vec::new(),
        console: Vec::new(),
    }
}

fn table(value: Option<&Value>) -> Vec<(String, String)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, item)| {
                (key.clone(), item.as_str().unwrap_or_default().to_owned())
            })
            .collect(),
        _ => Vec::new(),
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
