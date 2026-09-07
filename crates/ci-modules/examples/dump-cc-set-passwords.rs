//! `cc_set_passwords.handle`'s decisions, for the differential harness. Paired
//! with `tests/differential/ccsetpw.py`.
//!
//! Only the plan is printed. Carrying it out changes the passwords on the
//! developer's own machine, so the Python side stubs `chpasswd`,
//! `passwd --expire`, `multi_log`, `update_ssh_config` and `manage_service`
//! out and records them in the order they were made.
//!
//! `dump-cc-set-passwords <cfg-json> <state-json> <default-user-json> <args-json>`
//!
//! `<state-json>` is what the module reads off the running system:
//!
//! * `service` — `distro.get_option("ssh_svcname", "ssh")`.
//! * `systemd` — `distro.uses_systemd()`.
//! * `updated` — whether `update_ssh_config` would rewrite `sshd_config`.
//! * `active` — `systemctl show --property ActiveState --value <service>`.
//!
//! Generated passwords are not random here: `rand_user_password` is replaced
//! on both sides by a counter, because the whole point is to compare which
//! users got one and where it was announced.

use ci_config::{Object, Value};
use ci_log::Logger;
use ci_modules::cc::set_passwords::{plan, State, Step};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(cfg), Some(state), Some(default_user), Some(extra)) =
        (args.get(1), args.get(2), args.get(3), args.get(4))
    else {
        eprintln!(
            "usage: dump-cc-set-passwords <cfg-json> <state-json> \
             <default-user-json> <args-json>"
        );
        std::process::exit(2);
    };
    let parse = |text: &str| match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-cc-set-passwords: {err}");
            std::process::exit(2);
        }
    };
    let Value::Object(cfg) = parse(cfg) else {
        eprintln!("dump-cc-set-passwords: <cfg-json> must be an object");
        std::process::exit(2);
    };
    let state = state_of(&parse(state));
    let default_user = parse(default_user);
    let default_user = match &default_user {
        Value::Null => None,
        other => Some(other),
    };
    let extra = parse(extra);

    let mut counter = 0;
    let mut rand = move || {
        counter += 1;
        format!("<random {counter}>")
    };

    let mut log = Logger::silent();
    let printed = match plan(&cfg, &state, &extra, default_user, &mut rand, &mut log) {
        Ok(steps) => Value::Array(steps.iter().map(step).collect()),
        Err(error) => {
            let mut out = Object::new();
            out.insert("error".to_owned(), Value::String(error));
            Value::Object(out)
        }
    };
    println!("{}", ci_core::jsonfmt::dumps_indent(&printed, 1));
}

fn state_of(value: &Value) -> State {
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    let text = |key: &str, fallback: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()
    };
    State {
        ssh_svcname: text("service", "ssh"),
        systemd: flag("systemd"),
        sshd_config_changes: flag("updated"),
        ssh_active_state: text("active", ""),
    }
}

/// `dumps_indent` sorts every mapping it prints; the Python side asks
/// `json.dumps` for `sort_keys=True`.
fn step(step: &Step) -> Value {
    let mut out = Object::new();
    let op = |out: &mut Object, name: &str| {
        out.insert("op".to_owned(), Value::String(name.to_owned()));
    };
    match step {
        Step::Chpasswd { entries, hashed } => {
            op(&mut out, "chpasswd");
            out.insert("hashed".to_owned(), Value::Bool(*hashed));
            out.insert(
                "entries".to_owned(),
                Value::Array(
                    entries
                        .iter()
                        .map(|(name, secret)| {
                            Value::Array(vec![name.clone(), secret.clone()])
                        })
                        .collect(),
                ),
            );
        }
        Step::AnnounceRandom(text) => {
            op(&mut out, "announce_random");
            out.insert("text".to_owned(), Value::String(text.clone()));
        }
        Step::ExpirePasswd(user) => {
            op(&mut out, "expire_passwd");
            out.insert("user".to_owned(), user.clone());
        }
        Step::UpdateSshConfig { value } => {
            op(&mut out, "update_ssh_config");
            out.insert("value".to_owned(), Value::String(value.clone()));
        }
        Step::RestartSsh {
            service,
            ignore_dependencies,
        } => {
            op(&mut out, "restart_ssh");
            out.insert(
                "ignore_dependencies".to_owned(),
                Value::Bool(*ignore_dependencies),
            );
            out.insert("service".to_owned(), Value::String(service.clone()));
        }
        Step::Abort(message) => {
            op(&mut out, "abort");
            out.insert("error".to_owned(), Value::String(message.clone()));
        }
    }
    Value::Object(out)
}
