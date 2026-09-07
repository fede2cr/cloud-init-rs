//! `cc_timezone.handle`'s decisions, for the differential harness. Paired with
//! `tests/differential/tz.py`.
//!
//! Usage: `dump-cc-timezone <cfg-json> <distro> <zone-exists> <localtime> <systemd>`
//!        `dump-cc-timezone --batch <cases-file>`
//!
//! where `<zone-exists>` is `0` or `1`, `<localtime>` is `symlink`, `regular`
//! or `absent`, and `<systemd>` is `0` or `1`. Those three are the facts about
//! the live filesystem the decision turns on, supplied rather than read so
//! that the comparison covers combinations the host is not in — and so that
//! neither side relinks the `/etc/localtime` of the machine running it.
//!
//! Batch mode takes one tab-separated argument list per line and emits a
//! `## <line>` marker before each record, so that the whole matrix costs one
//! process on each side instead of one per case.

use ci_config::{Object, Value};
use ci_distro::timezone::{plan, LocalTime, Step};
use ci_modules::cc::timezone::configured;

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-timezone: cannot read {:?}", arg(2));
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

    if argv.len() < 6 {
        eprintln!(
            "usage: dump-cc-timezone <cfg-json> <distro> <zone-exists> \
             <localtime> <systemd> | --batch <cases>"
        );
        return std::process::ExitCode::from(2);
    }
    let fields: Vec<&str> = (1..6).map(arg).collect();
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
    std::process::ExitCode::SUCCESS
}

fn one(fields: &[&str]) -> Value {
    let at = |index: usize| fields.get(index).copied().unwrap_or("");
    let mut out = Object::new();

    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(at(0)) else {
        out.insert(
            "error".to_owned(),
            Value::String("<cfg-json> must be an object".to_owned()),
        );
        return Value::Object(out);
    };
    let Some(distro) = ci_distro::fetch(at(1)) else {
        out.insert(
            "error".to_owned(),
            Value::String("unknown distro".to_owned()),
        );
        return Value::Object(out);
    };
    let localtime = match at(3) {
        "symlink" => LocalTime::Symlink,
        "regular" => LocalTime::Regular,
        _ => LocalTime::Absent,
    };

    match configured(&Value::Null, &cfg) {
        None => {
            out.insert("tz".to_owned(), Value::Null);
            out.insert("calls".to_owned(), Value::Array(Vec::new()));
        }
        Some(tz) => {
            out.insert("tz".to_owned(), Value::String(tz.clone()));
            match plan(distro, &tz, at(2) == "1", localtime, at(4) == "1") {
                Ok(steps) => {
                    out.insert(
                        "calls".to_owned(),
                        Value::Array(steps.iter().map(call).collect()),
                    );
                }
                Err(error) => {
                    out.insert("error".to_owned(), Value::String(error));
                }
            }
        }
    }
    Value::Object(out)
}

fn call(step: &Step) -> Value {
    let mut out = Object::new();
    let mut put = |key: &str, value: &str| {
        out.insert(key.to_owned(), Value::String(value.to_owned()));
    };
    match step {
        Step::WriteName { path, contents } => {
            put("op", "write_file");
            put("path", path);
            put("content", contents);
        }
        Step::Remove { path } => {
            put("op", "del_file");
            put("path", path);
        }
        Step::Link { target, path } => {
            put("op", "symlink");
            put("source", target);
            put("link", path);
        }
        Step::Copy { from, to } => {
            put("op", "copy");
            put("dest", to);
            put("src", from);
        }
    }
    Value::Object(out)
}
