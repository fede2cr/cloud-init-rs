//! Port of `cloudinit/config/cc_users_groups.py`.
//!
//! The module that makes the machine usable: it turns `users:` and `groups:`
//! into accounts someone can log into. Everything it does is done by
//! `ci-distro` — normalising the config, planning a `create_user`, running it
//! — so what is left here is the glue, and the glue is where the refusals
//! live.
//!
//! There are three of them, and all three abort the whole module rather than
//! skipping one user, because upstream raises. That is deliberate: a boot that
//! half-applied a `users:` block would leave an account whose keys never
//! arrived, which is worse than no account at all.
//!
//! Split into a [`plan`] and a loop over it for the same reason `create_user`
//! is: the decisions are pure and can be compared against upstream
//! byte-for-byte, while running them needs a machine.

use ci_config::{option, Object, Value};
use ci_distro::ug;
use ci_log::Logger;

use super::{py_str, Args};

const SOURCE: &str = "cc_users_groups.py";

/// Keys that say the account has no home directory.
const NO_HOME: [&str; 2] = ["no_create_home", "system"];

/// Keys that need one. Mutually exclusive with [`NO_HOME`].
const NEED_HOME: [&str; 3] =
    ["ssh_authorized_keys", "ssh_import_id", "ssh_redirect_user"];

/// One `cloud.distro.*` call the module decided to make, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    CreateGroup { name: String, members: Vec<Value> },
    CreateUser { name: String, config: Object },
}

/// Everything `handle` decides before it touches the system.
///
/// `default_user` is `distro.get_default_user()`, i.e. `system_info`'s
/// `default_user` block; `cloud_keys` is `cloud.get_public_ssh_keys()`, read
/// only to be handed to a redirected user.
pub fn plan(
    cfg: &Object,
    default_user: Option<&Value>,
    cloud_keys: &[String],
    log: &mut Logger,
) -> Result<Vec<Call>, String> {
    let ug::Normalized { mut users, groups } =
        ug::normalize_users_groups(cfg, default_user, log)?;
    // Pops the flag out of `users` as well as reading it, which is why it has
    // to happen before the loop below copies each config out.
    let default_user = ug::extract_default(&mut users).map(|(name, _)| name);

    let mut calls: Vec<Call> = groups
        .iter()
        .map(|(name, members)| Call::CreateGroup {
            name: name.clone(),
            members: members.as_array().cloned().unwrap_or_default(),
        })
        .collect();

    for (user, config) in &users {
        let mut config = config.as_object().cloned().unwrap_or_default();
        let present = |keys: &[&str], config: &Object| -> Vec<String> {
            keys.iter()
                .filter(|key| config.get(**key).is_some_and(option::py_truthy))
                .map(|key| (*key).to_owned())
                .collect()
        };
        let no_home = present(&NO_HOME, &config);
        let need_home = present(&NEED_HOME, &config);
        if !no_home.is_empty() && !need_home.is_empty() {
            return Err(format!(
                "Not creating user {user}. Key(s) {} cannot be provided with {}",
                need_home.join(", "),
                no_home.join(", ")
            ));
        }

        let redirect = config
            .shift_remove("ssh_redirect_user")
            .unwrap_or(Value::Bool(false));
        if option::py_truthy(&redirect) {
            if config.contains_key("ssh_authorized_keys")
                || config.contains_key("ssh_import_id")
            {
                return Err(format!(
                    "Not creating user {user}. ssh_redirect_user cannot be \
                     provided with ssh_import_id or ssh_authorized_keys"
                ));
            }
            if !is_true_or_default(&redirect) {
                return Err(format!(
                    "Not creating user {user}. Invalid value of ssh_redirect_user: {}. \
                     Expected values: true, default or false.",
                    py_str(&redirect)
                ));
            }
            match &default_user {
                None => log.warning(
                    SOURCE,
                    &format!(
                        "Ignoring ssh_redirect_user: {} for {user}. No default_user \
                         defined. Perhaps missing cloud configuration users:  \
                         [default, ..].",
                        py_str(&redirect)
                    ),
                ),
                Some(default_user) => {
                    // Not the redirecting user's own keys: the *cloud's*, so
                    // that whoever holds the launch key gets the refusal
                    // message naming the account they should have used.
                    config.insert(
                        "ssh_redirect_user".to_owned(),
                        Value::String(default_user.clone()),
                    );
                    config.insert(
                        "cloud_public_ssh_keys".to_owned(),
                        cloud_keys
                            .iter()
                            .map(|k| Value::String(k.clone()))
                            .collect(),
                    );
                }
            }
        }

        calls.push(Call::CreateUser {
            name: user.clone(),
            config,
        });
    }
    Ok(calls)
}

/// `ssh_redirect_user not in (True, "default")`, negated.
///
/// Python compares by value, and `1 == True`, so a config that says
/// `ssh_redirect_user: 1` is accepted where `2` is not.
fn is_true_or_default(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() == Some(1.0),
        Value::String(text) => text == "default",
        _ => false,
    }
}

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let default_user = args.system_info.get("default_user").cloned();
    let cloud_keys: Vec<String> = args
        .datasource
        .map_or_else(Vec::new, |ds| ds.public_keys.to_vec());
    let calls = plan(args.cfg, default_user.as_ref(), &cloud_keys, args.logger)?;

    // One probe for the whole module rather than one per account: upstream
    // asks per call, but the answer cannot change while this loop runs.
    let snappy = ci_core::sysinfo::system_is_snappy(args.root);
    for call in calls {
        match call {
            Call::CreateGroup { name, members } => ci_distro::create::create_group(
                &Value::String(name),
                &members,
                args.root,
                snappy,
                args.logger,
            )?,
            Call::CreateUser { name, config } => {
                let state = ci_distro::create::State {
                    user_exists: ci_sys::ids::passwd_entry(args.root, &name).is_some(),
                    shadow_password_is_empty:
                        ci_distro::create::shadow_password_is_empty(
                            args.distro.shadow_fn,
                            args.distro.shadow_empty_locked_passwd_patterns,
                            args.root,
                            &name,
                            snappy,
                            args.logger,
                        ),
                    snappy,
                };
                let steps =
                    ci_distro::create::plan(&name, &config, state, args.logger)?;
                ci_distro::create::run(&steps, args.root, args.logger)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan_of(cfg: &Value, default_user: Option<&Value>) -> Result<Vec<Call>, String> {
        let mut log = Logger::silent();
        plan(
            cfg.as_object().unwrap(),
            default_user,
            &["ssh-rsa CLOUD".to_owned()],
            &mut log,
        )
    }

    fn user_config(calls: &[Call], want: &str) -> Object {
        calls
            .iter()
            .find_map(|call| match call {
                Call::CreateUser { name, config } if name == want => {
                    Some(config.clone())
                }
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn groups_are_created_before_any_user() {
        let calls = plan_of(
            &json!({"groups": ["staff"], "users": [{"name": "alice"}]}),
            None,
        )
        .unwrap();
        assert!(matches!(calls[0], Call::CreateGroup { .. }));
        assert!(matches!(calls[1], Call::CreateUser { .. }));
    }

    #[test]
    fn a_system_account_may_not_ask_for_keys() {
        let error = plan_of(
            &json!({"users": [{"name": "alice", "system": true,
                               "ssh_authorized_keys": ["ssh-rsa A"]}]}),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Not creating user alice. Key(s) ssh_authorized_keys cannot be provided \
             with system"
        );
    }

    #[test]
    fn a_redirect_is_replaced_by_the_default_users_name_and_the_clouds_keys() {
        let calls = plan_of(
            &json!({"users": ["default", {"name": "root", "ssh_redirect_user": true}]}),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap();
        let root = user_config(&calls, "root");
        assert_eq!(root["ssh_redirect_user"], json!("ubuntu"));
        assert_eq!(root["cloud_public_ssh_keys"], json!(["ssh-rsa CLOUD"]));
    }

    #[test]
    fn a_redirect_with_no_default_user_is_dropped_not_fatal() {
        let calls = plan_of(
            &json!({"users": [{"name": "root", "ssh_redirect_user": true}]}),
            None,
        )
        .unwrap();
        let root = user_config(&calls, "root");
        assert!(!root.contains_key("ssh_redirect_user"));
        assert!(!root.contains_key("cloud_public_ssh_keys"));
    }

    #[test]
    fn a_redirect_that_names_something_else_is_refused() {
        let error = plan_of(
            &json!({"users": [{"name": "root", "ssh_redirect_user": "ubuntu"}]}),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Not creating user root. Invalid value of ssh_redirect_user: ubuntu. \
             Expected values: true, default or false."
        );
    }

    #[test]
    fn one_is_true_enough_for_a_redirect() {
        let calls = plan_of(
            &json!({"users": ["default", {"name": "root", "ssh_redirect_user": 1}]}),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap();
        assert_eq!(
            user_config(&calls, "root")["ssh_redirect_user"],
            json!("ubuntu")
        );
    }

    #[test]
    fn a_redirect_cannot_be_combined_with_keys_of_its_own() {
        let error = plan_of(
            &json!({"users": [{"name": "root", "ssh_redirect_user": true,
                               "ssh_import_id": ["lp:someone"]}]}),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap_err();
        assert!(
            error.starts_with("Not creating user root. ssh_redirect_user cannot be")
        );
    }

    #[test]
    fn the_default_user_no_longer_carries_the_flag_that_named_it() {
        let calls = plan_of(
            &json!({"users": ["default"]}),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap();
        let ubuntu = user_config(&calls, "ubuntu");
        assert!(!ubuntu.contains_key("default"));
    }
}
