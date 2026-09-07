//! Port of the user and group half of `cloudinit/distros/__init__.py`:
//! `create_group`, `add_user`, and the sudo and doas rules that come with them.
//!
//! The split here is the same one [`crate::hostname`] uses, and for the same
//! reason. Deciding what to run is a pure function of the config and is
//! checked against upstream directly; running it is a handful of guarded
//! `subp` calls. That matters more here than anywhere else in the port,
//! because the thing being decided is the `useradd` argument list — one wrong
//! or missing flag and the account either cannot log in or can do more than it
//! should, and neither shows up as a failure at the time.
//!
//! Three of the rules below are sharper than they look:
//!
//! * Only *string* values become `useradd` options. `gecos: 5` is silently
//!   dropped, while `uid: 5` survives because upstream stringifies that one
//!   key by hand first.
//! * A `groups:` mapping is iterated for its keys, so `groups: {sudo: null}`
//!   really does add the user to `sudo`.
//! * `no_create_home` and `system` both mean `-M`, and `system` additionally
//!   means the account is exempt from the home-directory keys that
//!   `cc_users_groups` refuses to combine with it.

use ci_config::repr::type_name;
use ci_config::{option, Object, Value};
use ci_log::Logger;
use regex::Regex;

const SOURCE: &str = "distros/__init__.py";

/// `--comment`-style options, keyed by config name. Sorted, because upstream
/// walks `sorted(kwargs.items())` and the argument order is observable.
const USERADD_OPTS: [(&str, &str); 10] = [
    ("expiredate", "--expiredate"),
    ("gecos", "--comment"),
    ("groups", "--groups"),
    ("homedir", "--home"),
    ("inactive", "--inactive"),
    ("passwd", "--password"),
    ("primary_group", "--gid"),
    ("selinux_user", "--selinux-user"),
    ("shell", "--shell"),
    ("uid", "--uid"),
];

/// Options whose presence alone is the argument.
const USERADD_FLAGS: [(&str, &str); 3] = [
    ("no_log_init", "--no-log-init"),
    ("no_user_group", "--no-user-group"),
    ("system", "--system"),
];

/// The one option whose value must never reach a log.
const REDACT: [&str; 1] = ["passwd"];

/// A resolved `useradd` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Useradd {
    /// The command as it will be run.
    pub argv: Vec<String>,
    /// The same command with `--password` blanked, which is what upstream's
    /// `logstring` puts in the log. The two differ by exactly one word and
    /// that word is a password hash.
    pub log_argv: Vec<String>,
    /// Supplementary groups the user is being put into, plus the primary
    /// group if one was named. These are created first when they do not
    /// already exist.
    ///
    /// Values rather than strings: the supplementary ones have to be strings
    /// to survive the comma join, but the primary group is appended *after*
    /// that join and is never checked, so `primary_group: 5` reaches
    /// `groupadd` as an integer.
    pub groups: Vec<Value>,
    /// `create_groups`, which defaults to true and is consumed rather than
    /// passed to `useradd`.
    pub create_groups: bool,
}

/// `Distro.add_user`'s argument construction, without running anything.
pub fn useradd(
    name: &str,
    config: &Object,
    snappy: bool,
    log: &mut Logger,
) -> Result<Useradd, String> {
    let create_groups = config.get("create_groups").is_none_or(option::py_truthy);

    let mut argv = vec!["useradd".to_owned(), name.to_owned()];
    let mut log_argv = argv.clone();
    if snappy {
        argv.push("--extrausers".to_owned());
        log_argv.push("--extrausers".to_owned());
    }

    // `groups` is rewritten in place before the option loop reads it, so the
    // value that reaches `--groups` is the normalised comma string, not what
    // the config said.
    let mut config = config.clone();
    let mut groups: Vec<String> = Vec::new();
    let mut named: Vec<Value> = Vec::new();
    if config.get("groups").is_some_and(option::py_truthy) {
        let raw = config.get("groups").cloned().unwrap_or(Value::Null);
        let pieces: Vec<Value> = match &raw {
            Value::String(text) => text
                .split(',')
                .map(|piece| Value::String(piece.to_owned()))
                .collect(),
            Value::Array(items) => items.clone(),
            Value::Object(map) => {
                deprecate(
                    log,
                    &format!(
                        "The user {name} has a 'groups' config value of type dict is \
                         deprecated in 22.3 and scheduled to be removed in 27.3. Use a \
                         comma-delimited string or array instead: group1,group2."
                    ),
                );
                map.keys().map(|k| Value::String(k.clone())).collect()
            }
            other => {
                return Err(format!("'{}' object is not iterable", type_name(other)))
            }
        };
        for piece in pieces {
            let Value::String(text) = piece else {
                return Err(format!(
                    "'{}' object has no attribute 'strip'",
                    type_name(&piece)
                ));
            };
            groups.push(text.trim().to_owned());
        }
        let joined = groups.join(",");
        named = groups.into_iter().map(Value::String).collect();
        config.insert("groups".to_owned(), Value::String(joined));
        // The primary group is created alongside the supplementary ones but is
        // not part of `--groups`, which is why it is appended after the join —
        // and why it never has to be a string.
        if let Some(primary) = config.get("primary_group") {
            if option::py_truthy(primary) {
                named.push(primary.clone());
            }
        }
    }

    // `uid` is the one numeric option upstream stringifies, so it is also the
    // only one that survives being written as a number in the config.
    if let Some(uid) = config.get("uid") {
        let text = py_str(uid);
        config.insert("uid".to_owned(), Value::String(text));
    }

    let mut keys: Vec<&String> = config.keys().collect();
    keys.sort();
    for key in keys {
        let Some(value) = config.get(key) else {
            continue;
        };
        if let Some((_, opt)) = USERADD_OPTS.iter().find(|(name, _)| name == key) {
            // Non-strings are dropped here without a word, which is how a
            // `gecos: 5` goes missing.
            let Value::String(text) = value else { continue };
            if text.is_empty() {
                continue;
            }
            argv.push((*opt).to_owned());
            argv.push(text.clone());
            log_argv.push((*opt).to_owned());
            log_argv.push(if REDACT.contains(&key.as_str()) {
                "REDACTED".to_owned()
            } else {
                text.clone()
            });
        } else if let Some((_, flag)) =
            USERADD_FLAGS.iter().find(|(name, _)| name == key)
        {
            if option::py_truthy(value) {
                argv.push((*flag).to_owned());
                log_argv.push((*flag).to_owned());
            }
        }
    }

    let homeless = config.get("no_create_home").is_some_and(option::py_truthy)
        || config.get("system").is_some_and(option::py_truthy);
    let home_flag = if homeless { "-M" } else { "-m" };
    argv.push(home_flag.to_owned());
    log_argv.push(home_flag.to_owned());

    Ok(Useradd {
        argv,
        log_argv,
        groups: named,
        create_groups,
    })
}

/// `str(value)` for the values `useradd` stringifies.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => ci_config::repr::repr(other),
    }
}

/// `Distro.create_group`'s command, without running it.
///
/// Adding the members is a separate `usermod -a -G` per member, and upstream
/// skips any member that does not already exist rather than creating it — so a
/// group whose members are listed before their own `users:` entries comes out
/// empty.
#[must_use]
pub fn groupadd(name: &Value, snappy: bool) -> Vec<Value> {
    let mut argv = vec![Value::String("groupadd".to_owned()), name.clone()];
    if snappy {
        argv.push(Value::String("--extrausers".to_owned()));
    }
    argv
}

/// `Distro.write_sudo_rules`' content, without the file it goes in.
///
/// Every line is `<user> <rule>`, so a rule containing a newline writes more
/// than one sudoers line and the second one is not prefixed with the user.
/// Upstream does not check for that and neither does this; `cc_users_groups`
/// is not the place it would be caught.
pub fn sudo_rules(user: &str, rules: &Value) -> Result<String, String> {
    let mut lines = vec![String::new(), format!("# User rules for {user}")];
    match rules {
        Value::Array(items) => {
            for rule in items {
                lines.push(format!("{user} {}", py_str(rule)));
            }
        }
        Value::String(text) => lines.push(format!("{user} {text}")),
        other => {
            return Err(format!(
                "Can not create sudoers rule addition with type '{}'",
                type_name(other)
            ))
        }
    }
    lines.push(String::new());
    Ok(lines.join("\n"))
}

/// `Distro.is_doas_rule_valid`.
///
/// The rule must name the user it is being written for. A rule that parses but
/// names someone else is rejected, and one that does not parse at all is
/// rejected too — which is the right way round, since the file it would land
/// in grants root.
///
/// A non-string rule is an error rather than a rejection: upstream hands the
/// value straight to `re.search`, which refuses it. That distinction is worth
/// keeping, because a rejection writes no doas file while an error aborts
/// `create_user` altogether.
pub fn is_doas_rule_valid(user: &str, rule: &Value) -> Result<bool, String> {
    let Value::String(text) = rule else {
        return Err(format!(
            "expected string or bytes-like object, got '{}'",
            type_name(rule)
        ));
    };
    Ok(doas_rule_user(text).is_some_and(|named| named == user))
}

/// The identifier a doas rule grants to, if the rule matches upstream's
/// pattern.
///
/// Transcribed character for character from `Distro.is_doas_rule_valid`'s
/// regular expression rather than reimplemented. An earlier hand-written
/// tokeniser here accepted `permit alice garbage`, which upstream rejects:
/// the pattern is anchored at both ends, so anything after the optional
/// `as`/`cmd`/`args` clauses invalidates the whole rule. Getting that wrong in
/// the permissive direction would have written a doas file upstream refuses
/// to write.
fn doas_rule_user(rule: &str) -> Option<String> {
    // A compile failure here would be a bug in a literal, and is covered by a
    // unit test. Should it ever happen anyway, every rule becomes invalid and
    // no doas file is written, which is the safe direction for a file that
    // grants root.
    static PATTERN: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();
    let re = PATTERN
        .get_or_init(|| {
            Regex::new(concat!(
                r"^(?:permit|deny)",
                r"(?:\s+(?:nolog|nopass|persist|keepenv|setenv \{[^}]+\})+)*",
                r"\s+([a-zA-Z0-9_]+)+",
                r"(?:\s+as\s+[a-zA-Z0-9_]+)*",
                r"(?:\s+cmd\s+[^\s]+(?:\s+args\s+[^\s]+(?:\s*[^\s]+)*)*)*",
                r"\s*$",
            ))
            .ok()
        })
        .as_ref()?;
    re.captures(rule)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `Distro.write_doas_rules`' content.
///
/// Returns `None` when any rule fails validation, because upstream writes
/// *nothing* in that case rather than the subset that passed.
pub fn doas_rules(
    user: &str,
    rules: &[Value],
    log: &mut Logger,
) -> Result<Option<String>, String> {
    for rule in rules {
        if !is_doas_rule_valid(user, rule)? {
            log.log(
                ci_log::Level::Error,
                SOURCE,
                &format!(
                    "Invalid doas rule {} for user '{user}', not writing any doas rules for user!",
                    ci_config::repr::repr(rule)
                ),
            );
            return Ok(None);
        }
    }
    let mut lines = vec![String::new(), format!("# cloud-init User rules for {user}")];
    for rule in rules {
        lines.push(py_str(rule));
    }
    lines.push(String::new());
    Ok(Some(lines.join("\n")))
}

/// `lifecycle.deprecate`, which logs against its own module.
pub(crate) fn deprecate(log: &mut Logger, message: &str) {
    log.log(ci_log::Level::Deprecated, "lifecycle.py", message);
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

    fn cmd(config: &Value) -> Vec<String> {
        let mut log = Logger::silent();
        useradd("alice", &obj(config), false, &mut log)
            .unwrap()
            .argv
    }

    #[test]
    fn the_bare_case_creates_a_home() {
        assert_eq!(cmd(&json!({})), ["useradd", "alice", "-m"]);
    }

    #[test]
    fn a_system_user_gets_no_home() {
        assert_eq!(
            cmd(&json!({"system": true})),
            ["useradd", "alice", "--system", "-M"]
        );
    }

    #[test]
    fn options_come_out_in_key_order() {
        assert_eq!(
            cmd(
                &json!({"shell": "/bin/zsh", "gecos": "Alice", "homedir": "/srv/alice"})
            ),
            [
                "useradd",
                "alice",
                "--comment",
                "Alice",
                "--home",
                "/srv/alice",
                "--shell",
                "/bin/zsh",
                "-m"
            ]
        );
    }

    #[test]
    fn a_non_string_option_is_dropped_but_uid_is_not() {
        // `gecos: 5` never reaches `useradd`; `uid: 5` does, because upstream
        // stringifies that one key before the loop looks at it.
        assert_eq!(
            cmd(&json!({"gecos": 5, "uid": 1005})),
            ["useradd", "alice", "--uid", "1005", "-m"]
        );
    }

    #[test]
    fn groups_are_trimmed_and_the_primary_group_is_created_too() {
        let mut log = Logger::silent();
        let out = useradd(
            "alice",
            &obj(&json!({"groups": "sudo, docker", "primary_group": "staff"})),
            false,
            &mut log,
        )
        .unwrap();
        assert!(out
            .argv
            .windows(2)
            .any(|w| w == ["--groups", "sudo,docker"]));
        assert_eq!(out.groups, ["sudo", "docker", "staff"]);
    }

    #[test]
    fn a_groups_mapping_is_read_for_its_keys() {
        let mut log = Logger::silent();
        let out = useradd(
            "alice",
            &obj(&json!({"groups": {"sudo": null, "docker": null}})),
            false,
            &mut log,
        )
        .unwrap();
        assert_eq!(out.groups, ["sudo", "docker"]);
    }

    #[test]
    fn the_password_is_redacted_from_the_log_form_only() {
        let mut log = Logger::silent();
        let out = useradd(
            "alice",
            &obj(&json!({"passwd": "$6$hash"})),
            false,
            &mut log,
        )
        .unwrap();
        assert!(out.argv.contains(&"$6$hash".to_owned()));
        assert!(!out.log_argv.contains(&"$6$hash".to_owned()));
        assert!(out.log_argv.contains(&"REDACTED".to_owned()));
    }

    #[test]
    fn sudo_rules_prefix_every_line_with_the_user() {
        assert_eq!(
            sudo_rules("alice", &json!(["ALL=(ALL) NOPASSWD:ALL", "ALL=(ALL) /bin/ls"])).unwrap(),
            "\n# User rules for alice\nalice ALL=(ALL) NOPASSWD:ALL\nalice ALL=(ALL) /bin/ls\n"
        );
        assert_eq!(
            sudo_rules("alice", &json!("ALL=(ALL) NOPASSWD:ALL")).unwrap(),
            "\n# User rules for alice\nalice ALL=(ALL) NOPASSWD:ALL\n"
        );
        assert_eq!(
            sudo_rules("alice", &json!(5)).unwrap_err(),
            "Can not create sudoers rule addition with type 'int'"
        );
    }

    #[test]
    fn a_doas_rule_must_name_its_own_user() {
        let valid = |rule: &Value| is_doas_rule_valid("alice", rule).unwrap();
        assert!(valid(&json!("permit nopass alice")));
        assert!(valid(&json!("permit alice as root")));
        assert!(!valid(&json!("permit nopass bob")));
        assert!(!valid(&json!("nonsense")));
        assert_eq!(
            is_doas_rule_valid("alice", &json!(5)).unwrap_err(),
            "expected string or bytes-like object, got 'int'"
        );
    }

    #[test]
    fn one_bad_doas_rule_discards_all_of_them() {
        let mut log = Logger::silent();
        let rules = vec![json!("permit nopass alice"), json!("permit nopass bob")];
        assert_eq!(doas_rules("alice", &rules, &mut log).unwrap(), None);
    }

    #[test]
    fn a_snappy_groupadd_writes_to_the_extra_user_database() {
        let sudo = Value::String("sudo".to_owned());
        assert_eq!(groupadd(&sudo, false), ["groupadd", "sudo"]);
        assert_eq!(groupadd(&sudo, true), ["groupadd", "sudo", "--extrausers"]);
    }

    #[test]
    fn the_doas_pattern_compiles() {
        // `doas_rule_user` fails closed if its literal does not compile, so
        // nothing else in this file would notice a typo in it.
        assert_eq!(doas_rule_user("permit alice").as_deref(), Some("alice"));
    }
}
