//! `cc_mcollective` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccmcollective.py`.
//!
//! Usage: `dump-cc-mcollective <case-json>`
//!        `dump-cc-mcollective --batch <cases-file>`
//!
//! The case supplies the config, the files that already exist, the paths whose
//! read or copy fails with something other than `ENOENT`, and any call that
//! should fail. The record is the log, the ordered list of things the module
//! did, and what each file ended up holding.

use ci_config::{Object, Value};
use ci_modules::cc::mcollective::{handle_with, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-mcollective: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-mcollective <case-json>");
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
        .unwrap_or("mcollective")
        .to_owned();
    let cfg = object_of(case.get("cfg"));
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    let outcome = handle_with(&name, &cfg, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    out.insert("written".to_owned(), pairs(&host.written));
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
        calls: Vec::new(),
        written: Vec::new(),
        files: table(host.get("files")),
        failures: errnos(host.get("failures")),
        errors: table(host.get("errors")),
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

fn errnos(value: Option<&Value>) -> Vec<(String, i32)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(key, item)| {
                let code = item.as_i64()?;
                Some((key.clone(), i32::try_from(code).unwrap_or(0)))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn pairs(items: &[(String, String)]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(key, item)| {
                Value::Array(vec![
                    Value::String(key.clone()),
                    Value::String(item.clone()),
                ])
            })
            .collect(),
    )
}

fn strings(lines: &[String]) -> Value {
    Value::Array(lines.iter().cloned().map(Value::String).collect())
}
