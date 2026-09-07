//! `cc_snap.handle`'s decisions, for the differential harness. Paired with
//! `tests/differential/snap.py`.
//!
//! Usage: `dump-cc-snap <cfg-json> <snap-present>`
//!        `dump-cc-snap --batch <cases-file>`
//!
//! `<snap-present>` is `0` or `1`, standing in for `subp.which("snap")` —
//! supplied rather than read so the comparison covers both answers on a
//! machine that has snapd and on one that does not.
//!
//! Only the planning path is compared. Upstream reports a failed command as
//! `str(ProcessExecutionError)`, a six-line template this port does not
//! reproduce, so no case here makes a command fail.
//!
//! Batch mode takes one tab-separated argument list per line and emits a
//! `## <line>` marker before each record.

use ci_config::{Object, Value};
use ci_modules::cc::snap::{plan, Command, Step};

/// `cloud.paths.get_ipath_cur()`, pinned so both sides agree without either
/// having to have booted.
const IPATH: &str = "/var/lib/cloud/instance";

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-snap: cannot read {:?}", arg(2));
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

    if argv.len() < 3 {
        eprintln!("usage: dump-cc-snap <cfg-json> <snap-present> | --batch <cases>");
        return std::process::ExitCode::from(2);
    }
    let fields: Vec<&str> = (1..3).map(arg).collect();
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
    let present = at(1) == "1";

    let assertions_file = format!("{IPATH}/snapd.assertions");
    let (steps, failed) = plan(&cfg, &assertions_file, "snap");

    let mut calls: Vec<Value> = Vec::new();
    for step in &steps {
        calls.extend(emitted(step, present));
    }

    out.insert("calls".to_owned(), Value::Array(calls));
    if let Some(error) = failed {
        out.insert("error".to_owned(), Value::String(error));
    }
    Value::Object(out)
}

fn logged(level: &str, message: &str) -> Value {
    let mut out = Object::new();
    out.insert("message".to_owned(), Value::String(message.to_owned()));
    out.insert("op".to_owned(), Value::String(level.to_owned()));
    Value::Object(out)
}

fn emitted(step: &Step, present: bool) -> Vec<Value> {
    // `args` is a list for an argv command and a bare string for a shell one,
    // because that is exactly what upstream hands `subp`.
    let subp = |args: Value, shell: bool| {
        let mut out = Object::new();
        out.insert("args".to_owned(), args);
        out.insert("op".to_owned(), Value::String("subp".to_owned()));
        out.insert("shell".to_owned(), Value::Bool(shell));
        Value::Object(out)
    };
    let words = |items: &[&str]| {
        Value::Array(
            items
                .iter()
                .map(|w| Value::String((*w).to_owned()))
                .collect(),
        )
    };

    match step {
        Step::Debug(message) => vec![logged("debug", message)],
        Step::Warning(message) => vec![logged("warning", message)],
        Step::WaitSeeded => {
            let mut run = Object::new();
            run.insert("freq".to_owned(), Value::String("once".to_owned()));
            run.insert("name".to_owned(), Value::String("snap-seeded".to_owned()));
            run.insert("op".to_owned(), Value::String("cloud_run".to_owned()));
            let mut out = vec![Value::Object(run)];
            if present {
                out.push(subp(
                    words(&["snap", "wait", "system", "seed.loaded"]),
                    false,
                ));
            } else {
                out.push(logged(
                    "debug",
                    "Skipping snap wait, no snap command present",
                ));
            }
            out
        }
        Step::WriteAssertions { path, contents } => {
            let mut write = Object::new();
            write.insert("content".to_owned(), Value::String(contents.clone()));
            write.insert("op".to_owned(), Value::String("write_file".to_owned()));
            write.insert("path".to_owned(), Value::String(path.clone()));
            vec![Value::Object(write)]
        }
        Step::Ack { path } => vec![subp(words(&["snap", "ack", path]), false)],
        Step::Command(Command::Argv(argv)) => {
            vec![subp(Value::Array(argv.clone()), false)]
        }
        Step::Command(Command::Shell(source)) => {
            vec![subp(Value::String(source.clone()), true)]
        }
    }
}
