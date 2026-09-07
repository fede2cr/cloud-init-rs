//! `cc_locale.handle`'s decisions, for the differential harness. Paired with
//! `tests/differential/loc.py`.
//!
//! Usage: `dump-cc-locale <cfg-json> <distro> <system-locale> <conf-exists>
//! <locale-gen> <update-locale>`
//!        `dump-cc-locale --batch <cases-file>`
//!
//! `<system-locale>` is `-` for "the conf file does not set `LANG`". The last
//! three are `0` or `1`. Everything the decision depends on is supplied rather
//! than read, because carrying the plan out would run `locale-gen` on the
//! machine doing the comparison.
//!
//! Batch mode takes one tab-separated argument list per line and emits a
//! `## <line>` marker before each record, so that the whole matrix costs one
//! process on each side instead of one per case.

use ci_config::{Object, Value};
use ci_distro::locale::{plan, State, Step};
use ci_log::Logger;
use ci_modules::cc::locale::{config_file, configured};

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-cc-locale: cannot read {:?}", arg(2));
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

    if argv.len() < 7 {
        eprintln!(
            "usage: dump-cc-locale <cfg-json> <distro> <system-locale> \
             <conf-exists> <locale-gen> <update-locale> | --batch <cases>"
        );
        return std::process::ExitCode::from(2);
    }
    let fields: Vec<&str> = (1..7).map(arg).collect();
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
    let state = State {
        system_locale: (at(2) != "-").then(|| at(2).to_owned()),
        conf_fn_exists: at(3) == "1",
        has_locale_gen: at(4) == "1",
        has_update_locale: at(5) == "1",
    };

    let locale = configured(&Value::Null, &cfg, distro, state.system_locale.as_deref());
    out.insert("locale".to_owned(), Value::String(locale.clone()));

    if ci_config::option::is_false(&Value::from(locale.clone())) {
        out.insert("skipped".to_owned(), Value::Bool(true));
        out.insert("calls".to_owned(), Value::Array(Vec::new()));
    } else {
        out.insert("skipped".to_owned(), Value::Bool(false));
        let out_fn = config_file(&cfg);
        let out_fn = out_fn.as_deref();
        match plan(distro, &locale, out_fn, &state, &mut Logger::silent()) {
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
    Value::Object(out)
}

fn call(step: &Step) -> Value {
    let mut out = Object::new();
    match step {
        Step::InstallPackages { packages } => {
            out.insert(
                "op".to_owned(),
                Value::String("install_packages".to_owned()),
            );
            out.insert(
                "packages".to_owned(),
                Value::Array(packages.iter().map(|p| Value::from(p.clone())).collect()),
            );
        }
        Step::LocaleGen { locale } => {
            out.insert("op".to_owned(), Value::String("subp".to_owned()));
            out.insert(
                "argv".to_owned(),
                Value::Array(vec![
                    Value::from("locale-gen"),
                    Value::from(locale.clone()),
                ]),
            );
        }
        Step::UpdateLocale { argv } => {
            out.insert("op".to_owned(), Value::String("subp".to_owned()));
            out.insert(
                "argv".to_owned(),
                Value::Array(argv.iter().map(|a| Value::from(a.clone())).collect()),
            );
        }
    }
    Value::Object(out)
}
