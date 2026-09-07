//! Dump `shlex.split` and `util.load_shell_content`, for differential testing.
//!
//! Usage: `dump-shlex <file>`
//!
//! The file holds one base64 blob per line, so a case can carry newlines,
//! quotes and backslashes without the shell in between having an opinion.

use ci_config::{Object, Value};

fn outcome<T>(
    result: Result<T, ci_core::shlex::Error>,
    ok: impl Fn(T) -> Value,
) -> Value {
    match result {
        Ok(value) => ok(value),
        Err(err) => Value::String(format!("ValueError: {err}")),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dump-shlex <file>");
        std::process::exit(2);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("cannot read {path}");
        std::process::exit(2);
    };

    let mut out = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let Some(raw) = ci_core::b64::decode(line) else {
            continue;
        };
        let input = String::from_utf8_lossy(&raw).into_owned();

        let mut case = Object::new();
        case.insert("input".to_owned(), Value::from(input.as_str()));
        for (key, comments) in [("split", false), ("split_comments", true)] {
            case.insert(
                key.to_owned(),
                outcome(ci_core::shlex::split(&input, comments), |tokens| {
                    Value::Array(tokens.into_iter().map(Value::String).collect())
                }),
            );
        }
        case.insert(
            "shell_content".to_owned(),
            outcome(ci_core::shlex::load_shell_content(&input), |data| {
                Value::Object(
                    data.into_iter()
                        .map(|(key, value)| (key, Value::String(value)))
                        .collect(),
                )
            }),
        );
        out.push(Value::Object(case));
    }

    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Array(out), 1));
}
