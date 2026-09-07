//! `cc_ca_certs.handle`'s decisions, for the differential harness. Paired with
//! `tests/differential/ccca.py`.
//!
//! Two modes, because the module has two comparable surfaces:
//!
//! * `plan <cfg-json> <distro>` — the ordered list of actions. Carrying them
//!   out would empty the trust store of the machine running the comparison,
//!   so the Python side stubs the four escapes and records them instead.
//! * `deselect <file>` — the rewrite of `ca-certificates.conf`, run for real
//!   against a scratch file so the early exits for a missing and an empty
//!   file are compared too.

use ci_config::{Object, Value};
use ci_log::Logger;
use ci_modules::cc::ca_certs::{plan, run, Step};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);
    match arg(1) {
        "plan" => dump_plan(arg(2), arg(3)),
        "deselect" => dump_deselect(arg(2)),
        other => {
            eprintln!("dump-cc-ca-certs: unknown mode {other:?}");
            std::process::ExitCode::from(2)
        }
    }
}

fn dump_plan(cfg: &str, distro: &str) -> std::process::ExitCode {
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(cfg) else {
        eprintln!("dump-cc-ca-certs: <cfg-json> must be an object");
        return std::process::ExitCode::from(2);
    };
    let mut log = Logger::silent();
    let printed = match plan("ca_certs", &cfg, distro, &mut log) {
        Ok(steps) => Value::Array(steps.iter().map(step).collect()),
        Err(error) => {
            let mut out = Object::new();
            out.insert("error".to_owned(), Value::String(error));
            Value::Object(out)
        }
    };
    println!("{}", ci_core::jsonfmt::dumps_indent(&printed, 1));
    std::process::ExitCode::SUCCESS
}

/// The path is absolute and the root is `/`, so the step lands on the scratch
/// file itself.
fn dump_deselect(path: &str) -> std::process::ExitCode {
    let steps = [Step::DisableSystemCaCerts {
        path: path.to_owned(),
    }];
    let mut log = Logger::silent();
    if let Err(error) = run(&steps, std::path::Path::new("/"), &mut log) {
        eprintln!("dump-cc-ca-certs: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let mut out = Object::new();
    out.insert(
        "content".to_owned(),
        match std::fs::read_to_string(path) {
            Ok(text) => Value::String(text),
            Err(_) => Value::Null,
        },
    );
    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
    std::process::ExitCode::SUCCESS
}

fn step(step: &Step) -> Value {
    let mut out = Object::new();
    let mut op = |name: &str| {
        out.insert("op".to_owned(), Value::String(name.to_owned()));
    };
    match step {
        Step::RemoveDirContents { path } => {
            op("remove_dir_contents");
            out.insert("path".to_owned(), Value::String(path.clone()));
        }
        Step::DisableSystemCaCerts { path } => {
            op("disable_system_ca_certs");
            out.insert("path".to_owned(), Value::String(path.clone()));
        }
        Step::DebconfSetSelections { data } => {
            op("debconf_set_selections");
            out.insert("data".to_owned(), Value::String(data.clone()));
        }
        Step::WriteCert { path, contents } => {
            op("write_cert");
            out.insert("contents".to_owned(), Value::String(contents.clone()));
            out.insert("path".to_owned(), Value::String(path.clone()));
        }
        Step::UpdateCaCerts { argv } => {
            op("update_ca_certs");
            out.insert(
                "argv".to_owned(),
                Value::Array(argv.iter().map(|arg| Value::from(arg.clone())).collect()),
            );
        }
    }
    Value::Object(out)
}
