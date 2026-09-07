//! `ug_util.normalize_users_groups` + `extract_default`, for the differential
//! harness. Paired with `tests/differential/ugutil.py`.

use ci_config::{Object, Value};
use ci_log::Logger;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(cfg), Some(default_user)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: dump-ug <cfg-json> <default-user-json>");
        std::process::exit(2);
    };
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(cfg) else {
        eprintln!("dump-ug: <cfg-json> must be an object");
        std::process::exit(2);
    };
    let default_user: Value = match serde_json::from_str(default_user) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-ug: {err}");
            std::process::exit(2);
        }
    };
    let default_user = match &default_user {
        Value::Null => None,
        other => Some(other),
    };

    let mut log = Logger::silent();
    let mut out = Object::new();
    match ci_distro::ug::normalize_users_groups(&cfg, default_user, &mut log) {
        Ok(mut normalized) => {
            let default = ci_distro::ug::extract_default(&mut normalized.users);
            let (name, config) = match default {
                Some((name, config)) => (Value::String(name), Value::Object(config)),
                None => (Value::Null, Value::Null),
            };
            out.insert("users".to_owned(), Value::Object(normalized.users));
            out.insert("groups".to_owned(), Value::Object(normalized.groups));
            out.insert("default_name".to_owned(), name);
            out.insert("default_config".to_owned(), config);
        }
        Err(error) => {
            out.insert("error".to_owned(), Value::String(error));
        }
    }
    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
}
