//! `cc_apt_pipelining.handle`'s decisions, for the differential harness.
//! Paired with `tests/differential/aptpipe.py`.
//!
//! Usage: `dump-cc-apt-pipelining <cfg-json>`
//!        `dump-cc-apt-pipelining --batch <cases-file>`
//!
//! The module writes into `/etc/apt/apt.conf.d`, which is a real directory on
//! the machine running the comparison, so neither side carries the plan out:
//! the Python side stubs `util.write_file` and records the call, and this
//! prints the same call from [`decide`] and [`render`].
//!
//! In batch mode each line of the case file is one case's argument list, tab
//! separated, and the output is a `## <line>` marker plus that case's record.
//! One process for the whole matrix rather than one per case: the Python side
//! pays a third of a second of interpreter and `import cloudinit` startup every
//! time it is spawned, which across a few thousand cases is essentially the
//! entire cost of the comparison.

use ci_config::{Object, Value};
use ci_modules::cc::apt_pipelining::{decide, render, Action, DEFAULT_FILE};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-apt-pipelining: cannot read {:?}", arg(2));
            return std::process::ExitCode::from(2);
        };
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            println!("## {line}");
            println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
        }
        return std::process::ExitCode::SUCCESS;
    }

    if argv.len() < 2 {
        eprintln!("usage: dump-cc-apt-pipelining <cfg-json> | --batch <cases>");
        return std::process::ExitCode::from(2);
    }
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(&[arg(1)]), 1));
    std::process::ExitCode::SUCCESS
}

fn one(fields: &[&str]) -> Value {
    let mut out = Object::new();
    let text = fields.first().copied().unwrap_or("");
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(text) else {
        out.insert(
            "error".to_owned(),
            Value::String("<cfg-json> must be an object".to_owned()),
        );
        return Value::Object(out);
    };

    let mut calls = Vec::new();
    match decide(cfg.get("apt_pipelining")) {
        Action::Write(setting) => {
            calls.push(call(&[
                ("op", "write_file"),
                ("path", DEFAULT_FILE),
                ("content", &render(&setting)),
            ]));
            calls.push(call(&[
                ("op", "debug"),
                (
                    "message",
                    &format!(
                        "Wrote {DEFAULT_FILE} with apt pipeline depth setting {setting}"
                    ),
                ),
            ]));
        }
        Action::Leave => {}
        Action::Invalid(shown) => {
            calls.push(call(&[
                ("op", "warning"),
                (
                    "message",
                    &format!("Invalid option for apt_pipelining: {shown}"),
                ),
            ]));
        }
    }

    out.insert("calls".to_owned(), Value::Array(calls));
    Value::Object(out)
}

fn call(fields: &[(&str, &str)]) -> Value {
    let mut out = Object::new();
    for (key, value) in fields {
        out.insert((*key).to_owned(), Value::String((*value).to_owned()));
    }
    Value::Object(out)
}
