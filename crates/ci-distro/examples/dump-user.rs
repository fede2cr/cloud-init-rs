//! The pure halves of `Distro.add_user`, `write_sudo_rules`,
//! `write_doas_rules` and `create_user`, for the differential harness. Paired
//! with `tests/differential/user.py`.
//!
//! The `create` mode prints the password it would have set. That is fine here
//! and nowhere else: the harness feeds it fixture strings, and the sequence is
//! only worth comparing if the argument is part of it.

use ci_config::{Object, Value};
use ci_distro::create::{State, Step};
use ci_log::Logger;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(mode), Some(name), Some(payload)) =
        (args.get(1), args.get(2), args.get(3))
    else {
        eprintln!("usage: dump-user <argv|sudo|doas|create> <name> <json> [flags]");
        std::process::exit(2);
    };
    let payload: Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-user: {err}");
            std::process::exit(2);
        }
    };

    let mut log = Logger::silent();
    let mut out = Object::new();
    match mode.as_str() {
        "argv" => {
            let Value::Object(config) = payload else {
                eprintln!("dump-user: argv payload must be an object");
                std::process::exit(2);
            };
            let snappy = args.get(4).is_some_and(|flag| flag == "1");
            match ci_distro::user::useradd(name, &config, snappy, &mut log) {
                // Groups are created before the user, so the recorded calls
                // run groupadd first and useradd last — the order upstream's
                // recorder sees.
                Ok(useradd) => {
                    let mut calls = Vec::new();
                    if useradd.create_groups {
                        for group in &useradd.groups {
                            let group_argv = ci_distro::user::groupadd(group, snappy);
                            let mut map = Object::new();
                            map.insert(
                                "argv".to_owned(),
                                Value::Array(group_argv.clone()),
                            );
                            map.insert("log".to_owned(), Value::Array(group_argv));
                            calls.push(Value::Object(map));
                        }
                    }
                    calls.push(call(&useradd.argv, &useradd.log_argv));
                    out.insert("calls".to_owned(), Value::Array(calls));
                }
                Err(err) => {
                    out.insert("error".to_owned(), Value::String(err));
                }
            }
        }
        "sudo" => match ci_distro::user::sudo_rules(name, &payload) {
            Ok(content) => {
                out.insert("content".to_owned(), Value::String(content));
            }
            Err(err) => {
                out.insert("error".to_owned(), Value::String(err));
            }
        },
        "doas" => {
            let Value::Array(rules) = payload else {
                eprintln!("dump-user: doas payload must be an array");
                std::process::exit(2);
            };
            let valid: Result<Vec<Value>, String> = rules
                .iter()
                .map(|rule| {
                    ci_distro::user::is_doas_rule_valid(name, rule).map(Value::Bool)
                })
                .collect();
            match valid.and_then(|valid| {
                ci_distro::user::doas_rules(name, &rules, &mut log)
                    .map(|content| (valid, content))
            }) {
                Ok((valid, content)) => {
                    out.insert(
                        "content".to_owned(),
                        content.map_or(Value::Null, Value::String),
                    );
                    out.insert("valid".to_owned(), Value::Array(valid));
                }
                Err(err) => {
                    out.insert("error".to_owned(), Value::String(err));
                }
            }
        }
        "create" => create(name, payload, &args, &mut log, &mut out),
        _ => {
            eprintln!("dump-user: unknown mode {mode}");
            std::process::exit(2);
        }
    }

    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Object(out), 1));
}

/// The `create` mode: the whole ordered sequence, without running any of it.
fn create(
    name: &str,
    payload: Value,
    args: &[String],
    log: &mut Logger,
    out: &mut Object,
) {
    let Value::Object(config) = payload else {
        eprintln!("dump-user: create payload must be an object");
        std::process::exit(2);
    };
    let flag = |i: usize| args.get(i).is_some_and(|f| f == "1");
    let state = State {
        user_exists: flag(4),
        shadow_password_is_empty: flag(5),
        snappy: flag(6),
    };
    match ci_distro::create::plan(name, &config, state, log) {
        Ok(steps) => {
            let steps = steps.iter().map(step).collect();
            out.insert("steps".to_owned(), Value::Array(steps));
        }
        Err(err) => {
            out.insert("error".to_owned(), Value::String(err));
        }
    }
}

/// One planned step, in the shape the Python recorder produces.
fn step(step: &Step) -> Value {
    let mut map = Object::new();
    let mut put = |key: &str, value: Value| {
        map.insert(key.to_owned(), value);
    };
    let strings = |items: &[String]| {
        Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
    };
    match step {
        Step::CreateGroup { argv, .. } | Step::AddSnapUser { argv } => {
            put("argv", Value::Array(argv.clone()));
            put("log", Value::Array(argv.clone()));
            put("op", Value::String("run".to_owned()));
        }
        Step::AddUser(add) => {
            put("argv", strings(&add.argv));
            put("log", strings(&add.log_argv));
            put("op", Value::String("run".to_owned()));
        }
        Step::SetPasswd {
            user,
            passwd,
            hashed,
        } => {
            put("hashed", Value::Bool(*hashed));
            put("op", Value::String("set_passwd".to_owned()));
            put("passwd", Value::String(passwd.clone()));
            put("user", Value::String(user.clone()));
        }
        Step::LockPasswd(user) => {
            put("op", Value::String("lock_passwd".to_owned()));
            put("user", Value::String(user.clone()));
        }
        Step::UnlockPasswd(user) => {
            put("op", Value::String("unlock_passwd".to_owned()));
            put("user", Value::String(user.clone()));
        }
        Step::WriteDoasRules { user, rules } => {
            put("op", Value::String("write_doas_rules".to_owned()));
            put("rules", Value::Array(rules.clone()));
            put("user", Value::String(user.clone()));
        }
        Step::WriteSudoRules { user, rules } => {
            put("op", Value::String("write_sudo_rules".to_owned()));
            put("rules", rules.clone());
            put("user", Value::String(user.clone()));
        }
        Step::SetupUserKeys {
            user,
            keys,
            options,
        } => {
            put("keys", strings(keys));
            put("op", Value::String("setup_user_keys".to_owned()));
            put("options", Value::String(options.clone()));
            put("user", Value::String(user.clone()));
        }
    }
    Value::Object(map)
}

fn call(argv: &[String], log_argv: &[String]) -> Value {
    let mut map = Object::new();
    map.insert(
        "argv".to_owned(),
        Value::Array(argv.iter().map(|a| Value::String(a.clone())).collect()),
    );
    map.insert(
        "log".to_owned(),
        Value::Array(log_argv.iter().map(|a| Value::String(a.clone())).collect()),
    );
    Value::Object(map)
}
