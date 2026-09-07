//! Dumps a module section at each stage of resolution, for differential
//! testing against `tests/differential/modules.py`.
//!
//! `dump-modules <config.json> <section> <distro>`

use ci_config::Value;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), Some(section), Some(distro)) =
        (args.next(), args.next(), args.next())
    else {
        eprintln!("usage: dump-modules <config.json> <section> <distro>");
        std::process::exit(2);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read config: {err}");
            std::process::exit(2);
        }
    };
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(&text) else {
        eprintln!("config is not a JSON object");
        std::process::exit(2);
    };
    let section = section.to_string_lossy().into_owned();
    let distro = distro.to_string_lossy().into_owned();

    // Every log line the engine produces goes to stderr, which the harness does
    // not compare; the three stages below are what it does compare.
    let mut logger = ci_log::Logger::basic(ci_log::Level::Debug);
    let raw = ci_modules::read_modules(&cfg, &section, &mut logger);
    let fixed = ci_modules::fixup(&raw, &mut logger);
    let active = ci_modules::select(fixed.clone(), &cfg, &distro, &mut logger);

    let mut out = ci_config::Object::new();
    out.insert(
        "raw".to_owned(),
        Value::Array(raw.iter().map(raw_json).collect()),
    );
    out.insert(
        "fixed".to_owned(),
        Value::Array(fixed.iter().map(details_json).collect()),
    );
    out.insert(
        "active".to_owned(),
        Value::Array(active.iter().map(details_json).collect()),
    );
    println!("{}", ci_core::jsonfmt::json_dumps(&Value::Object(out)));
}

/// One `_read_modules` dict. Upstream omits the keys it did not set, so the
/// shape of the dict is itself part of what is compared.
fn raw_json(raw: &ci_modules::Raw) -> Value {
    let mut map = ci_config::Object::new();
    map.insert("mod".to_owned(), Value::String(raw.name.clone()));
    if let Some(freq) = &raw.frequency {
        map.insert("freq".to_owned(), Value::String(freq.clone()));
    }
    // `args` is only set when the entry carried one, and the list form only
    // sets it from the third element on.
    if let Some(args) = &raw.args {
        map.insert("args".to_owned(), args.clone());
    }
    Value::Object(map)
}

/// One `ModuleDetails`, with the module rendered as its import name.
fn details_json(details: &ci_modules::Details) -> Value {
    let mut map = ci_config::Object::new();
    map.insert(
        "module".to_owned(),
        Value::String(details.module.name.to_owned()),
    );
    map.insert("name".to_owned(), Value::String(details.name.clone()));
    map.insert(
        "frequency".to_owned(),
        Value::String(details.frequency.as_str().to_owned()),
    );
    map.insert("run_args".to_owned(), details.args.clone());
    Value::Object(map)
}
