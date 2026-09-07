//! `util.human2bytes`, for the differential harness. Paired with
//! `tests/differential/h2b.py`.
//!
//! Usage: `dump-human2bytes --batch <cases-file>`
//!        `dump-human2bytes <base64-of-size>`
//!
//! The size is passed base64-encoded because the interesting cases include
//! leading and trailing whitespace and the empty string, none of which survive
//! a plain line-oriented cases file.

use ci_config::{Object, Value};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-human2bytes: cannot read {:?}", arg(2));
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
        eprintln!("usage: dump-human2bytes <base64-size> | --batch <cases>");
        return std::process::ExitCode::from(2);
    }
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(arg(1)), 1));
    std::process::ExitCode::SUCCESS
}

fn one(encoded: &str) -> Value {
    let size = ci_core::b64::decode(encoded)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();

    let mut out = Object::new();
    match ci_core::human::human2bytes(&size) {
        Ok(bytes) => {
            out.insert("error".to_owned(), Value::Null);
            out.insert("result".to_owned(), Value::from(bytes));
        }
        Err(message) => {
            out.insert("error".to_owned(), Value::String(message));
            out.insert("result".to_owned(), Value::Null);
        }
    }
    out.insert("size".to_owned(), Value::String(size));
    Value::Object(out)
}
