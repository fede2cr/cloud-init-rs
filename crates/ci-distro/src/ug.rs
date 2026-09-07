//! Port of `cloudinit/distros/ug_util.py`: turning the many spellings of
//! `users:` and `groups:` into the one shape the rest of a boot understands.
//!
//! This is the most permissive input surface cloud-init has. A group list may
//! be a comma-separated string, a list of names, a list of one-key mappings,
//! or a mapping of name to members — and the members of each may themselves
//! be a string or a list. Users are worse: a mapping whose values are booleans
//! selects users by name, a mapping whose values are mappings configures them,
//! a list may mix bare names with configuration blocks, and the literal name
//! `default` is a placeholder for whatever the distro calls its first account.
//!
//! Everything here is pure. It decides *who* gets created and *with which
//! groups*, which is to say it decides who can log in; but it does not touch
//! the system, so it can be compared against upstream directly rather than
//! inferred from the users a test boot left behind. That is why it is split
//! out from the code that runs `useradd`.
//!
//! The one input that does not come from the config is the distro's own
//! default user, which upstream reads through `distro.get_default_user()` —
//! `system_info.default_user` in `cloud.cfg`. `ci-distro`'s [`Distro`] is a
//! static table row with nowhere to keep per-boot config, so callers pass that
//! block in.
//!
//! [`Distro`]: crate::Distro

use ci_config::repr::type_name;
use ci_config::{merge, option, Object, Value};
use ci_log::Logger;

const SOURCE: &str = "distros/ug_util.py";

/// `lifecycle.deprecate`, which logs against its own module rather than the
/// caller's.
fn deprecate(log: &mut Logger, message: &str) {
    log.log(ci_log::Level::Deprecated, "lifecycle.py", message);
}

/// The output of [`normalize_users_groups`].
///
/// Both maps preserve insertion order, which is the order upstream's dicts
/// have and the order the users and groups are then created in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalized {
    /// User name to that user's configuration. Exactly one entry carries
    /// `default: true`, if any does.
    pub users: Object,
    /// Group name to its member list.
    pub groups: Object,
}

/// `util.uniq_list`: order-preserving deduplication by equality.
fn uniq_list(items: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// `util.uniq_merge`.
///
/// A string source is split on commas and its empty pieces dropped, so
/// `"a, b"` contributes `"a"` and `" b"` — the spaces survive here and are
/// stripped later, or not at all. Anything that is neither a string nor a list
/// is what `list.extend` chokes on upstream.
pub fn uniq_merge(sources: &[&Value]) -> Result<Vec<Value>, String> {
    let mut combined: Vec<Value> = Vec::new();
    for source in sources {
        match source {
            Value::String(text) => {
                for piece in text.trim().split(',') {
                    if !piece.is_empty() {
                        combined.push(Value::String(piece.to_owned()));
                    }
                }
            }
            Value::Array(items) => combined.extend(items.iter().cloned()),
            Value::Object(map) => {
                // Iterating a dict yields its keys, which is what `extend`
                // does with one. Nothing in cloud-init means to rely on that,
                // but nothing rejects it either.
                combined.extend(map.keys().map(|k| Value::String(k.clone())));
            }
            other => {
                return Err(format!("'{}' object is not iterable", type_name(other)))
            }
        }
    }
    Ok(uniq_list(combined))
}

/// `sorted()` over the JSON values a config can produce.
///
/// Python compares strings to strings and numbers to numbers and refuses
/// everything else, so a member list that mixes them is an error rather than
/// an ordering. Booleans sort with the numbers, as they do upstream.
fn py_sorted(mut items: Vec<Value>) -> Result<Vec<Value>, String> {
    let strings = items.iter().filter(|v| v.is_string()).count();
    let numeric = items
        .iter()
        .filter(|v| v.is_number() || v.is_boolean())
        .count();
    if strings + numeric != items.len() || (strings != 0 && numeric != 0) {
        let mut names: Vec<&'static str> = Vec::new();
        for item in &items {
            let name = type_name(item);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let last = names.last().copied().unwrap_or("NoneType");
        let first = names.first().copied().unwrap_or("NoneType");
        return Err(format!(
            "'<' not supported between instances of '{last}' and '{first}'"
        ));
    }
    if strings == items.len() {
        items.sort_by(|a, b| a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or("")));
    } else {
        items.sort_by(|a, b| {
            num(a)
                .partial_cmp(&num(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    Ok(items)
}

fn num(value: &Value) -> f64 {
    match value {
        Value::Bool(true) => 1.0,
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `util.uniq_merge_sorted`.
fn uniq_merge_sorted(sources: &[&Value]) -> Result<Vec<Value>, String> {
    py_sorted(uniq_merge(sources)?)
}

/// `ug_util._normalize_groups`.
///
/// Returns group name to member list. Order is the order the names were first
/// seen; members inside each group are sorted, because upstream sorts them.
pub fn normalize_groups(grp_cfg: &Value) -> Result<Object, String> {
    // A string is a comma-separated list of group names with no members. Note
    // that upstream does *not* strip the pieces here, so `"a, b"` asks for a
    // group literally named `" b"`.
    let owned_list;
    let list: Option<&Vec<Value>> = match grp_cfg {
        Value::String(text) => {
            owned_list = text
                .trim()
                .split(',')
                .map(|piece| Value::String(piece.to_owned()))
                .collect::<Vec<_>>();
            Some(&owned_list)
        }
        Value::Array(items) => Some(items),
        _ => None,
    };

    let collapsed: Object = if let Some(items) = list {
        let mut acc = Object::new();
        for item in items {
            match item {
                Value::Object(map) => {
                    for (key, value) in map {
                        match value {
                            Value::Array(members) => {
                                append_members(&mut acc, key, members.clone());
                            }
                            Value::String(_) => {
                                append_members(&mut acc, key, vec![value.clone()]);
                            }
                            other => {
                                return Err(format!(
                                    "Bad group member type {}",
                                    type_name(other)
                                ))
                            }
                        }
                    }
                }
                Value::String(name) => {
                    if !acc.contains_key(name.as_str()) {
                        acc.insert(name.clone(), Value::Array(Vec::new()));
                    }
                }
                other => {
                    return Err(format!("Unknown group name type {}", type_name(other)))
                }
            }
        }
        acc
    } else if let Value::Object(map) = grp_cfg {
        map.clone()
    } else {
        return Err(format!(
            "Group config must be list, dict or string type only but found {}",
            type_name(grp_cfg)
        ));
    };

    let mut groups = Object::new();
    for (name, members) in &collapsed {
        groups.insert(name.clone(), Value::Array(uniq_merge_sorted(&[members])?));
    }
    Ok(groups)
}

fn append_members(acc: &mut Object, key: &str, members: Vec<Value>) {
    match acc.get_mut(key) {
        Some(Value::Array(existing)) => existing.extend(members),
        _ => {
            acc.insert(key.to_owned(), Value::Array(members));
        }
    }
}

/// `ug_util._normalize_users`.
///
/// `def_user_cfg` is the distro's `default_user` block. When it is present the
/// entry named `default` is renamed to whatever it calls the account, and that
/// user's groups become the union of the two sources — which is how the
/// image's `sudo` membership survives a user-data block that only meant to add
/// a key.
#[allow(clippy::too_many_lines)]
pub fn normalize_users(
    u_cfg: &Value,
    def_user_cfg: Option<&Object>,
) -> Result<Object, String> {
    // A mapping is two configs sharing a syntax: truthy scalars select a user
    // by name, mappings configure one. A false scalar drops the name outright.
    let entries: Vec<Value> = match u_cfg {
        Value::Object(map) => {
            let mut acc = Vec::new();
            for (key, value) in map {
                match value {
                    Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                        if option::is_true(value) {
                            acc.push(Value::String(key.clone()));
                        }
                    }
                    Value::Object(inner) => {
                        let mut inner = inner.clone();
                        inner.insert("name".to_owned(), Value::String(key.clone()));
                        acc.push(Value::Object(inner));
                    }
                    other => {
                        return Err(format!(
                            "Unmappable user value type {} for key {key}",
                            type_name(other)
                        ))
                    }
                }
            }
            acc
        }
        // Sorted, and the pieces are *not* stripped first, so `"alice, bob"`
        // sorts as `[" bob", "alice"]` and creates bob first. The strip
        // happens one loop down, which is why the names come out clean but in
        // an order nobody would have predicted.
        Value::String(_) => uniq_merge_sorted(&[u_cfg])?,
        Value::Array(items) => items.clone(),
        other => {
            return Err(format!(
                "User config must be dictionary/list or string  types only and not {}",
                type_name(other)
            ))
        }
    };

    let mut users = Object::new();
    for user_config in &entries {
        match user_config {
            Value::Array(_) | Value::String(_) => {
                for name in uniq_merge(&[user_config])? {
                    // Names are whatever the list held: `[[1, 2]]` really does
                    // ask for users named 1 and 2, and a falsy one — `0`, `""`,
                    // `null` — is dropped rather than named.
                    if !option::py_truthy(&name) {
                        continue;
                    }
                    let name = py_key(&name)?;
                    if !users.contains_key(&name) {
                        users.insert(name, Value::Object(Object::new()));
                    }
                }
            }
            Value::Object(map) => {
                let mut map = map.clone();
                // `pop("name", "default")`: an entry with no name configures
                // the placeholder, not a user called `name`.
                let name = match map.shift_remove("name") {
                    Some(Value::String(text)) => text,
                    Some(other) => py_key(&other)?,
                    None => "default".to_owned(),
                };
                let prev = match users.get(&name) {
                    Some(Value::Object(prev)) => prev.clone(),
                    _ => Object::new(),
                };
                users.insert(
                    name,
                    Value::Object(merge::merge_many(vec![prev, map], false)),
                );
            }
            other => {
                return Err(format!(
                "User config must be dictionary/list or string  types only and not {}",
                type_name(other)
            ))
            }
        }
    }

    // `ssh-authorized-keys` and `ssh_authorized_keys` are the same key; an
    // empty key after the rewrite is dropped rather than carried.
    let mut cleaned = Object::new();
    for (uname, uconfig) in &users {
        let mut c_uconfig = Object::new();
        if let Value::Object(map) = uconfig {
            for (key, value) in map {
                let key = key.replace('-', "_").trim().to_owned();
                if !key.is_empty() {
                    c_uconfig.insert(key, value.clone());
                }
            }
        }
        cleaned.insert(uname.clone(), Value::Object(c_uconfig));
    }
    let mut users = cleaned;

    let mut def_user: Option<String> = None;
    if let Some(Value::Object(def_config)) = users.shift_remove("default") {
        if let Some(def_user_cfg) = def_user_cfg {
            let mut def_user_cfg = def_user_cfg.clone();
            // No `name` here means the distro shipped a `default_user` block
            // without one, and upstream dies on the `pop`. Nothing sensible
            // can be built from it, so say which key is missing.
            let Some(name) = def_user_cfg.shift_remove("name") else {
                return Err("'name'".to_owned());
            };
            let name = match name {
                Value::String(text) => text,
                other => py_key(&other)?,
            };
            let def_groups = def_user_cfg
                .shift_remove("groups")
                .unwrap_or_else(|| Value::Array(Vec::new()));

            let mut parsed_config = match users.shift_remove(&name) {
                Some(Value::Object(map)) => map,
                _ => Object::new(),
            };
            let parsed_groups = parsed_config
                .get("groups")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let users_groups = uniq_merge_sorted(&[&parsed_groups, &def_groups])?;
            let joined = users_groups
                .iter()
                .map(|value| match value {
                    Value::String(text) => Ok(text.clone()),
                    other => Err(format!(
                        "sequence item 0: expected str instance, {} found",
                        type_name(other)
                    )),
                })
                .collect::<Result<Vec<String>, String>>()?
                .join(",");
            parsed_config.insert("groups".to_owned(), Value::String(joined));

            users.insert(
                name.clone(),
                Value::Object(merge::merge_many(
                    vec![def_user_cfg, def_config, parsed_config],
                    false,
                )),
            );
            def_user = Some(name);
        }
    }

    // Every user carries a `default` flag, so the flag being absent later
    // means the user came from somewhere other than this function.
    for (uname, uconfig) in &mut users {
        let is_default = def_user.as_ref().is_some_and(|name| name == uname);
        if let Value::Object(map) = uconfig {
            map.insert("default".to_owned(), Value::Bool(is_default));
        }
    }
    Ok(users)
}

/// A non-string `name:` used as a dict key, spelled the way it comes back out.
///
/// Upstream never converts these — the key really is the `None` or the `5` —
/// so the only place the spelling becomes visible is JSON, which renders
/// `None` as `null` and `True` as `true` rather than as their `repr`. A list
/// or a mapping cannot be a key at all.
fn py_key(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Null => Ok("null".to_owned()),
        Value::Bool(flag) => Ok(if *flag { "true" } else { "false" }.to_owned()),
        Value::Number(number) => Ok(number.to_string()),
        other => Err(format!(
            "cannot use '{kind}' as a dict key (unhashable type: '{kind}')",
            kind = type_name(other)
        )),
    }
}

/// `ug_util.normalize_users_groups`.
///
/// `default_user` is `distro.get_default_user()` — the `system_info` block,
/// which reaches modules beside their config rather than inside it.
#[allow(clippy::too_many_lines)]
pub fn normalize_users_groups(
    cfg: &Object,
    default_user: Option<&Value>,
    log: &mut Logger,
) -> Result<Normalized, String> {
    // The `user:` key is the pre-22.2 spelling. It does not just add a user:
    // it *becomes* the default, so it outranks whatever the image shipped.
    let mut old_user = Object::new();
    match cfg.get("user") {
        // A falsy value — absent, null, `false`, `""`, `{}` — is no key at all.
        None => {}
        Some(value) if !option::py_truthy(value) => {}
        Some(Value::String(name)) => {
            old_user.insert("name".to_owned(), Value::String(name.clone()));
            deprecate(
                log,
                "'user' of type string is deprecated in 22.2 and scheduled to be \
                 removed in 27.2. Use 'users' list instead.",
            );
        }
        Some(Value::Object(map)) => old_user.clone_from(map),
        Some(other) => {
            log.warning(
                SOURCE,
                &format!(
                    "Format for 'user' key must be a string or dictionary and not {}",
                    type_name(other)
                ),
            );
        }
    }
    let had_old_user = !old_user.is_empty();

    // `get_default_user` is meant to return a mapping. A falsy value stands in
    // for "this distro ships no default user"; anything else truthy makes
    // upstream die inside the merge, and dying is the right outcome — a
    // silently missing default user is an image nobody can log in to.
    let distro_user_config = match default_user {
        Some(Value::Object(map)) => Some(map.clone()),
        Some(other) if option::py_truthy(other) => {
            return Err(if other.is_array() {
                "pop expected at most 1 argument, got 2".to_owned()
            } else {
                format!("'{}' object has no attribute 'pop'", type_name(other))
            })
        }
        Some(_) | None => None,
    };
    let default_user_config = merge::merge_many(
        vec![
            old_user.clone(),
            distro_user_config.clone().unwrap_or_default(),
        ],
        false,
    );

    let mut base_users = cfg
        .get("users")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    match &base_users {
        Value::Array(_) => {}
        // Both of these still work; both are on a clock. The list spelling is
        // the only one that survives 27.2.
        Value::String(_) => deprecate(
            log,
            "'users' of type <class 'str'> is deprecated in 22.2 and scheduled \
             to be removed in 27.2. Use 'users' as a list.",
        ),
        Value::Object(_) => deprecate(
            log,
            "'users' of type <class 'dict'> is deprecated in 22.2 and scheduled \
             to be removed in 27.2. Use 'users' as a list.",
        ),
        other => {
            log.warning(
                SOURCE,
                &format!(
                    "Format for 'users' key must be a comma-separated string or a dictionary or a list but found {}",
                    type_name(other)
                ),
            );
            base_users = Value::Array(Vec::new());
        }
    }

    if had_old_user {
        match &mut base_users {
            Value::Array(items) => {
                let mut entry = Object::new();
                entry.insert("name".to_owned(), Value::String("default".to_owned()));
                items.push(Value::Object(entry));
            }
            Value::Object(map) => {
                let existing = map
                    .get("default")
                    .cloned()
                    .unwrap_or_else(|| Value::Bool(true));
                map.insert("default".to_owned(), existing);
            }
            Value::String(text) => {
                text.push_str(",default");
            }
            _ => {}
        }
    }

    let groups = match cfg.get("groups") {
        Some(value) => normalize_groups(value)?,
        None => Object::new(),
    };
    // The distro's default user only participates when there is a `default`
    // entry to rename; `users: []` on an image with a `default_user` creates
    // nobody, which is exactly how a config locks itself out.
    let users = normalize_users(
        &base_users,
        if distro_user_config.is_some() || had_old_user {
            Some(&default_user_config)
        } else {
            None
        },
    )?;
    Ok(Normalized { users, groups })
}

/// `ug_util.extract_default`: the user that carries `default: true`.
///
/// Upstream pops the flag out of the *live* config rather than a copy, so the
/// default user is the one user whose config no longer says `default` by the
/// time it reaches `create_user`, while everyone else still carries
/// `default: false`. Reproduced, mutation and all, because the difference is
/// visible in what gets passed on.
///
/// Returns `None` when nothing is marked, which is the case a caller has to
/// handle before it can honour `ssh_redirect_user`.
pub fn extract_default(users: &mut Object) -> Option<(String, Object)> {
    let name = users.iter().find_map(|(name, config)| {
        let map = config.as_object()?;
        map.get("default")
            .filter(|flag| option::py_truthy(flag))
            .map(|_| name.clone())
    })?;
    let config = users.get_mut(&name)?.as_object_mut()?;
    config.shift_remove("default");
    Some((name.clone(), config.clone()))
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

    fn obj(value: &Value) -> Object {
        value.as_object().unwrap().clone()
    }

    fn norm(cfg: &Value, default_user: Option<&Value>) -> Normalized {
        let mut log = Logger::silent();
        normalize_users_groups(&obj(cfg), default_user, &mut log).unwrap()
    }

    #[test]
    fn a_bare_list_of_names_becomes_empty_configs() {
        let out = norm(&json!({"users": ["alice", "bob"]}), None);
        assert_eq!(
            out.users,
            obj(&json!({"alice": {"default": false}, "bob": {"default": false}}))
        );
    }

    #[test]
    fn a_comma_string_splits_and_dedupes() {
        let out = norm(&json!({"users": "alice,bob,alice"}), None);
        assert_eq!(out.users.keys().collect::<Vec<_>>(), ["alice", "bob"]);
    }

    #[test]
    fn a_mapping_of_flags_selects_the_truthy_names() {
        let out = norm(&json!({"users": {"alice": true, "bob": false}}), None);
        assert_eq!(out.users.keys().collect::<Vec<_>>(), ["alice"]);
    }

    #[test]
    fn the_default_placeholder_takes_the_distros_name_and_groups() {
        let out = norm(
            &json!({"users": [{"name": "default", "shell": "/bin/zsh"},
                             {"name": "ubuntu", "groups": "docker"}]}),
            Some(
                &json!({"name": "ubuntu", "groups": ["adm", "sudo"], "shell": "/bin/bash"}),
            ),
        );
        let ubuntu = out.users.get("ubuntu").unwrap();
        // Groups are the union, sorted. The distro's shell wins over the
        // placeholder's: `mergemanydict` is first-wins and `def_user_cfg`
        // merges ahead of the `default` entry.
        assert_eq!(ubuntu.get("groups").unwrap(), "adm,docker,sudo");
        assert_eq!(ubuntu.get("shell").unwrap(), "/bin/bash");
        assert_eq!(ubuntu.get("default").unwrap(), true);
    }

    #[test]
    fn without_a_default_entry_the_distro_user_is_not_created() {
        let mut out = norm(
            &json!({"users": ["alice"]}),
            Some(&json!({"name": "ubuntu", "groups": ["sudo"]})),
        );
        assert_eq!(out.users.keys().collect::<Vec<_>>(), ["alice"]);
        assert!(extract_default(&mut out.users).is_none());
    }

    #[test]
    fn the_old_user_key_injects_a_default_entry() {
        let mut out = norm(
            &json!({"user": "azureuser", "users": []}),
            Some(&json!({"name": "ubuntu", "groups": ["sudo"]})),
        );
        // `user:` outranks the distro's name, so the account created is the
        // one the datasource asked for, not the one the image shipped.
        assert_eq!(out.users.keys().collect::<Vec<_>>(), ["azureuser"]);
        let (name, _) = extract_default(&mut out.users).unwrap();
        assert_eq!(name, "azureuser");
    }

    #[test]
    fn hyphenated_keys_are_rewritten() {
        let out = norm(
            &json!({"users": [{"name": "alice", "ssh-authorized-keys": ["k"]}]}),
            None,
        );
        let alice = out.users.get("alice").unwrap();
        assert!(alice.get("ssh_authorized_keys").is_some());
        assert!(alice.get("ssh-authorized-keys").is_none());
    }

    #[test]
    fn a_repeated_user_merges_first_wins() {
        let out = norm(
            &json!({"users": [{"name": "alice", "shell": "/bin/sh"},
                             {"name": "alice", "shell": "/bin/zsh", "uid": 1005}]}),
            None,
        );
        let alice = out.users.get("alice").unwrap();
        assert_eq!(alice.get("shell").unwrap(), "/bin/sh");
        assert_eq!(alice.get("uid").unwrap(), 1005);
    }

    #[test]
    fn groups_accept_every_spelling() {
        let string = normalize_groups(&json!("admin,dev")).unwrap();
        assert_eq!(string, obj(&json!({"admin": [], "dev": []})));

        let listed =
            normalize_groups(&json!(["admin", {"dev": ["bob", "alice"]}])).unwrap();
        assert_eq!(listed, obj(&json!({"admin": [], "dev": ["alice", "bob"]})));

        let mapped = normalize_groups(&json!({"dev": "bob,alice"})).unwrap();
        assert_eq!(mapped, obj(&json!({"dev": ["alice", "bob"]})));
    }

    #[test]
    fn a_repeated_group_accumulates_members() {
        let out =
            normalize_groups(&json!([{"dev": ["bob"]}, {"dev": "alice"}])).unwrap();
        assert_eq!(out, obj(&json!({"dev": ["alice", "bob"]})));
    }

    #[test]
    fn a_bad_group_member_type_is_an_error() {
        let err = normalize_groups(&json!([{"dev": 5}])).unwrap_err();
        assert_eq!(err, "Bad group member type int");
    }

    #[test]
    fn a_bad_group_config_type_is_an_error() {
        let err = normalize_groups(&json!(5)).unwrap_err();
        assert_eq!(
            err,
            "Group config must be list, dict or string type only but found int"
        );
    }

    #[test]
    fn extract_default_strips_the_flag() {
        let mut users =
            obj(&json!({"ubuntu": {"default": true, "shell": "/bin/bash"}}));
        let (name, config) = extract_default(&mut users).unwrap();
        assert_eq!(name, "ubuntu");
        assert_eq!(config, obj(&json!({"shell": "/bin/bash"})));
        // The flag is gone from the map too, not just from the return.
        assert_eq!(users, obj(&json!({"ubuntu": {"shell": "/bin/bash"}})));
    }
}
