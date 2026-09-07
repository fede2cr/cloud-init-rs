//! `cc_ssh_authkey_fingerprints` against a scripted machine.
//! Paired with `tests/differential/ccsshauthkeyfp.py`.
//!
//! Usage: `dump-cc-ssh-authkey-fp <case-json>`
//!        `dump-cc-ssh-authkey-fp --batch <cases-file>`
//!
//! The case supplies the config, the already-normalized user map, and what
//! `extract_authorized_keys` found for each user. The record is the log, which
//! users were asked about, and the exact lines that reached the console --
//! which is the whole point, because the module is nothing but formatting.

use ci_config::{Object, Value};
use ci_modules::cc::ssh_authkey_fingerprints::{handle_with, Entry, Fixture};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-ssh-authkey-fp: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-cc-ssh-authkey-fp <case-json>");
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
        .unwrap_or("ssh_authkey_fingerprints")
        .to_owned();
    let cfg = object_of(case.get("cfg"));
    let users = object_of(case.get("users"));
    let mut host = fixture(case.get("host"));
    let mut log = ci_log::Logger::capturing();

    handle_with(&name, &cfg, &users, &mut host, &mut log);
    out.insert("log".to_owned(), strings(log.captured()));
    out.insert("calls".to_owned(), strings(&host.calls));
    out.insert("console".to_owned(), strings(&host.console));
    Value::Object(out)
}

fn object_of(value: Option<&Value>) -> Object {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Object::new(),
    }
}

/// `"keys": {"<user>": ["<path>", [[keytype, base64, comment, options], ..]]}`
fn fixture(value: Option<&Value>) -> Fixture {
    let host = object_of(value);
    let keys = match host.get("keys") {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(user, found)| {
                let parts = found.as_array().cloned().unwrap_or_default();
                let path = parts
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let entries = parts
                    .get(1)
                    .and_then(Value::as_array)
                    .map(|rows| rows.iter().map(entry_of).collect())
                    .unwrap_or_default();
                (user.clone(), (path, entries))
            })
            .collect(),
        _ => Vec::new(),
    };
    Fixture {
        keys,
        calls: Vec::new(),
        console: Vec::new(),
    }
}

fn entry_of(row: &Value) -> Entry {
    let fields = row.as_array().cloned().unwrap_or_default();
    let field = |index: usize| {
        fields
            .get(index)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Entry {
        keytype: field(0),
        base64: field(1),
        comment: field(2),
        options: field(3),
    }
}

fn strings(lines: &[String]) -> Value {
    Value::Array(lines.iter().cloned().map(Value::String).collect())
}
