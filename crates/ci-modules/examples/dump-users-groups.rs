//! `cc_users_groups.handle`'s decisions, for the differential harness. Paired
//! with `tests/differential/usersgroups.py`.
//!
//! Only the plan is printed: carrying it out would add accounts to whatever
//! machine ran the harness, and the Python side records its two `distro` calls
//! rather than making them for exactly the same reason.

use ci_config::{Object, Value};
use ci_log::Logger;
use ci_modules::cc::users_groups::{plan, Call};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(cfg), Some(default_user), Some(cloud_keys)) =
        (args.get(1), args.get(2), args.get(3))
    else {
        eprintln!(
            "usage: dump-users-groups <cfg-json> <default-user-json> <cloud-keys-json>"
        );
        std::process::exit(2);
    };
    let Ok(Value::Object(cfg)) = serde_json::from_str::<Value>(cfg) else {
        eprintln!("dump-users-groups: <cfg-json> must be an object");
        std::process::exit(2);
    };
    let parse = |text: &str| match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-users-groups: {err}");
            std::process::exit(2);
        }
    };
    let default_user = parse(default_user);
    let default_user = match &default_user {
        Value::Null => None,
        other => Some(other),
    };
    let cloud_keys: Vec<String> = match parse(cloud_keys) {
        Value::Array(items) => items
            .iter()
            .filter_map(|key| key.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => Vec::new(),
    };

    let mut log = Logger::silent();
    let printed = match plan(&cfg, default_user, &cloud_keys, &mut log) {
        Ok(calls) => Value::Array(calls.iter().map(call).collect()),
        Err(error) => {
            let mut out = Object::new();
            out.insert("error".to_owned(), Value::String(error));
            Value::Object(out)
        }
    };
    println!("{}", ci_core::jsonfmt::dumps_indent(&printed, 1));
}

fn call(call: &Call) -> Value {
    let mut out = Object::new();
    match call {
        Call::CreateGroup { name, members } => {
            out.insert("members".to_owned(), Value::Array(members.clone()));
            out.insert("name".to_owned(), Value::String(name.clone()));
            out.insert("op".to_owned(), Value::String("create_group".to_owned()));
        }
        Call::CreateUser { name, config } => {
            out.insert("config".to_owned(), Value::Object(config.clone()));
            out.insert("name".to_owned(), Value::String(name.clone()));
            out.insert("op".to_owned(), Value::String("create_user".to_owned()));
        }
    }
    Value::Object(out)
}
