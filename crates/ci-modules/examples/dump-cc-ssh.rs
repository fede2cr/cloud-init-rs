//! `cc_ssh.handle`'s decisions, for the differential harness. Paired with
//! `tests/differential/ccssh.py`.
//!
//! Only the plan is printed. Carrying it out deletes every host key under
//! `/etc/ssh` and shells out to `ssh-keygen`, so the Python side stubs the
//! same six calls out and records them in the order they were made rather
//! than making them. What is compared is therefore the ordered list of things
//! the module *would* do — which is the part that decides whether the machine
//! comes up reachable, and by whom.
//!
//! `dump-cc-ssh <cfg-json> <state-json> <default-user-json> <cloud-keys-json>`
//!
//! `<state-json>` is what the module reads off the running system before it
//! decides anything:
//!
//! * `stale` — `glob("/etc/ssh/ssh_host_*key*")`, the keys the image shipped.
//! * `existing` — the paths for which `os.path.exists` answers true, which is
//!   how the generator decides a key type is already covered.
//! * `fips` — `util.fips_enabled()`.
//! * `redhat` — `distro.osfamily == "redhat"`.

use ci_config::{Object, Value};
use ci_log::Logger;
use ci_modules::cc::ssh::{plan, State, Step};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(cfg), Some(state), Some(default_user), Some(cloud_keys)) =
        (args.get(1), args.get(2), args.get(3), args.get(4))
    else {
        eprintln!(
            "usage: dump-cc-ssh <cfg-json> <state-json> <default-user-json> <cloud-keys-json>"
        );
        std::process::exit(2);
    };
    let parse = |text: &str| match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("dump-cc-ssh: {err}");
            std::process::exit(2);
        }
    };
    let Value::Object(cfg) = parse(cfg) else {
        eprintln!("dump-cc-ssh: <cfg-json> must be an object");
        std::process::exit(2);
    };
    let state = state_of(&parse(state));
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
    let printed = match plan(&cfg, &state, default_user, &cloud_keys, &mut log) {
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
    let strings = |key: &str| -> Vec<String> {
        match value.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    State {
        stale_key_files: strings("stale"),
        existing_key_files: strings("existing"),
        fips: flag("fips"),
        redhat: flag("redhat"),
    }
}

/// `dumps_indent` sorts every mapping it prints, so these go in whatever
/// order reads best; the Python side asks `json.dumps` for `sort_keys=True`.
fn step(step: &Step) -> Value {
    let mut out = Object::new();
    let op = |out: &mut Object, name: &str| {
        out.insert("op".to_owned(), Value::String(name.to_owned()));
    };
    match step {
        Step::DeleteFile(path) => {
            op(&mut out, "delete_file");
            out.insert("path".to_owned(), Value::String(path.clone()));
        }
        Step::WriteKey { path, mode, value } => {
            op(&mut out, "write_key");
            out.insert("mode".to_owned(), Value::String(format!("{mode:o}")));
            out.insert("path".to_owned(), Value::String(path.clone()));
            out.insert("value".to_owned(), value.clone());
        }
        Step::AppendSshConfig(lines) => {
            op(&mut out, "append_ssh_config");
            out.insert(
                "lines".to_owned(),
                Value::Array(
                    lines
                        .iter()
                        .map(|(key, value)| {
                            Value::Array(vec![
                                Value::String(key.clone()),
                                Value::String(value.clone()),
                            ])
                        })
                        .collect(),
                ),
            );
        }
        Step::KeygenPublic { private, public } => {
            op(&mut out, "keygen_public");
            out.insert("private".to_owned(), Value::String(private.clone()));
            out.insert("public".to_owned(), Value::String(public.clone()));
        }
        Step::Keygen {
            keytype,
            keyfile,
            quiet,
            redhat_perms,
        } => {
            op(&mut out, "keygen");
            out.insert("keyfile".to_owned(), Value::String(keyfile.clone()));
            out.insert("keytype".to_owned(), keytype.clone());
            out.insert("quiet".to_owned(), Value::Bool(*quiet));
            out.insert("redhat_perms".to_owned(), Value::Bool(*redhat_perms));
        }
        Step::PublishHostKeys { blacklist } => {
            op(&mut out, "publish_host_keys");
            out.insert("blacklist".to_owned(), Value::Array(blacklist.clone()));
        }
        Step::SetupUserKeys {
            user,
            keys,
            options,
        } => {
            op(&mut out, "setup_user_keys");
            out.insert("keys".to_owned(), Value::Array(keys.clone()));
            out.insert("options".to_owned(), Value::String(options.clone()));
            out.insert("user".to_owned(), Value::String(user.clone()));
        }
    }
    Value::Object(out)
}
