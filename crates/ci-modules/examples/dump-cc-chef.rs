//! `cc_chef` against a scripted machine, for the differential harness.
//! Paired with `tests/differential/ccchef.py`.
//!
//! Usage: `dump-cc-chef <case-json>`
//!        `dump-cc-chef --batch <cases-file>`
//!
//! The case supplies the config, the files and directories that already exist,
//! the paths `is_exe` answers yes for, what each URL serves, and any call that
//! should fail. The record is the log, the ordered list of things the module
//! did, and what each file ended up holding.
//!
//! `util.make_header()` embeds the current time, so the fixture answers with a
//! constant and the Python half patches it to the same one.

use ci_config::{Object, Value};
use ci_modules::cc::chef::{handle_with, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-chef: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-chef <case-json>");
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
        .unwrap_or("chef")
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
    let mut fixture = Fixture {
        files: table(host.get("files")),
        dirs: listings(host.get("dirs")),
        exes: strs(host.get("exes")),
        urls: table(host.get("urls")),
        errors: table(host.get("errors")),
        ..Fixture::default()
    };
    for (key, slot) in [
        ("templates_dir", &mut fixture.templates_dir),
        ("instance_id", &mut fixture.instance_id),
        ("header", &mut fixture.header),
        ("tmpdir", &mut fixture.tmpdir),
    ] {
        if let Some(text) = host.get(key).and_then(Value::as_str) {
            text.clone_into(slot);
        }
    }
    fixture
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

fn listings(value: Option<&Value>) -> Vec<(String, Vec<String>)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, item)| (key.clone(), strs(Some(item))))
            .collect(),
        _ => Vec::new(),
    }
}

fn strs(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().unwrap_or_default().to_owned())
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
