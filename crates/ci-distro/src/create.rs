//! Port of `Distro.create_user` — the half that decides what to do, and the
//! half that does it.
//!
//! [`crate::user`] builds the `useradd` argument list. This module is what
//! calls it, and then everything that comes after: the password, the sudoers
//! and doas files, and the SSH keys. It is the last link in the chain that
//! ends with an account someone can actually log into.
//!
//! The split is the same one used everywhere else in the port, but it earns
//! its keep here more than anywhere. [`plan`] turns the config and three facts
//! about the machine into an ordered list of [`Step`]s and touches nothing;
//! [`run`] executes that list. The reason is the password state machine below
//! — five flags feeding a four-armed decision — which is the part most likely
//! to be wrong and the part that cannot be observed after the fact. Getting it
//! wrong in the permissive direction leaves an account with a blank password
//! that anyone can walk into. As a pure function it is compared against
//! upstream case by case instead.
//!
//! Two behaviours here are worth knowing before reading the code:
//!
//! * `set_passwd` runs *before* the lock, so `lock_passwd: true` together with
//!   a password sets the password and then disables logging in with it.
//! * For a user that already exists, `passwd:` is ignored — only
//!   `plain_text_passwd` and `hashed_passwd` apply — but its mere presence
//!   still changes which message gets logged.

use std::path::Path;

use ci_config::repr::type_name;
use ci_config::{option, Object, Value};
use ci_log::{Level, Logger};

use crate::user::{self, py_str, Useradd};

const SOURCE: &str = "distros/__init__.py";

/// `Distro.ci_sudoers_fn`.
pub const CI_SUDOERS_FN: &str = "/etc/sudoers.d/90-cloud-init-users";

/// `Distro.shadow_fn`.
pub const SHADOW_FN: &str = "/etc/shadow";

/// The base file `ensure_sudo_dir` adds its `#includedir` to, and the
/// read-only copy it falls back to for content.
const SUDO_BASE: &str = "/etc/sudoers";
const SYSTEM_SUDO_BASE: &str = "/usr/etc/sudoers";

/// What `create_user` needs to know about the machine before it can decide
/// anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `util.is_user(name)`. When true, `add_user` returns immediately and
    /// none of the account's own settings — groups, shell, home — are applied.
    pub user_exists: bool,
    /// `_shadow_file_has_empty_user_password(name)`. Only consulted for a user
    /// that already exists and has no password in user-data.
    pub shadow_password_is_empty: bool,
    /// `util.system_is_snappy()`.
    pub snappy: bool,
}

/// One upstream call, in the order `create_user` makes them.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `create_group`, which skips the group if it is already there.
    ///
    /// A failure here is logged and stepped over, unlike everything else in
    /// the sequence — which is what makes a `primary_group: 5` cost the user
    /// its group rather than its account.
    CreateGroup {
        name: Value,
        argv: Vec<Value>,
    },
    /// `add_user`.
    AddUser(Box<Useradd>),
    /// `add_snap_user`, which replaces everything else.
    ///
    /// The arguments are values rather than strings because upstream appends
    /// `snapuser` unconverted, so a non-string one reaches `subp` and is
    /// refused there.
    AddSnapUser {
        argv: Vec<Value>,
    },
    /// `set_passwd`, fed on `chpasswd`'s stdin.
    SetPasswd {
        user: String,
        passwd: String,
        hashed: bool,
    },
    LockPasswd(String),
    UnlockPasswd(String),
    /// `write_doas_rules`, after the string or mapping has been iterated.
    WriteDoasRules {
        user: String,
        rules: Vec<Value>,
    },
    /// `write_sudo_rules`, which takes the value as it stands.
    WriteSudoRules {
        user: String,
        rules: Value,
    },
    /// `ssh_util.setup_user_keys`.
    SetupUserKeys {
        user: String,
        keys: Vec<String>,
        options: String,
    },
}

/// `Distro.create_user`, decided but not done.
///
/// The `Err` case is an exception upstream would have raised out of
/// `create_user` uncaught — a bad `groups` type, an unhashable key, a
/// `ssh_authorized_keys: null`. In every one of those the account is left
/// half-made, so the caller has to treat it as fatal for this user rather than
/// carry on.
pub fn plan(
    name: &str,
    config: &Object,
    state: State,
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();

    // A snap user is a different command and skips everything below it,
    // including the password and the keys.
    if config.contains_key("snapuser") {
        steps.push(snap_user(name, config, log));
        return Ok(steps);
    }

    let pre_existing = state.user_exists;
    if pre_existing {
        log.log(
            Level::Info,
            SOURCE,
            &format!("User {name} already exists, skipping."),
        );
    } else {
        let add = user::useradd(name, config, state.snappy, log)?;
        if add.create_groups {
            for group in &add.groups {
                steps.push(Step::CreateGroup {
                    name: group.clone(),
                    argv: user::groupadd(group, state.snappy),
                });
            }
        }
        steps.push(Step::AddUser(Box::new(add)));
    }

    password(name, config, state, pre_existing, &mut steps, log);

    if let Some(doas) = config.get("doas") {
        if option::py_truthy(doas) {
            steps.push(Step::WriteDoasRules {
                user: name.to_owned(),
                rules: py_iter(doas)?,
            });
        }
    }

    if let Some(sudo) = config.get("sudo") {
        if option::py_truthy(sudo) {
            steps.push(Step::WriteSudoRules {
                user: name.to_owned(),
                rules: sudo.clone(),
            });
        } else if *sudo == Value::Bool(false) {
            // `sudo: 0` does not land here: upstream tests `is False`.
            user::deprecate(
                log,
                &format!(
                    "The value of 'false' in user {name}'s 'sudo' config is \
                     deprecated in 22.2 and scheduled to be removed in 27.2. \
                     Use 'null' instead."
                ),
            );
        }
    }

    if let Some(raw) = config.get("ssh_authorized_keys") {
        steps.push(Step::SetupUserKeys {
            user: name.to_owned(),
            keys: key_list(raw, log)?,
            options: String::new(),
        });
    }

    if let Some(redirect) = config.get("ssh_redirect_user") {
        let cloud_keys = config
            .get("cloud_public_ssh_keys")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if option::py_truthy(&cloud_keys) {
            // Substituted with `str.replace`, which will not take a non-string
            // — so a `ssh_redirect_user: true` aborts here rather than
            // installing a banner naming "True".
            let Value::String(redirect) = redirect else {
                return Err(format!(
                    "replace() argument 2 must be str, not {}",
                    py_type(redirect)
                ));
            };
            let options = ci_ssh::disable_user_opts()
                .replace("$USER", redirect)
                .replace("$DISABLE_USER", name);
            steps.push(Step::SetupUserKeys {
                user: name.to_owned(),
                // Not normalised the way `ssh_authorized_keys` is: these go
                // straight into `set()`, so a bare string becomes one key per
                // character.
                keys: key_set(&py_iter(&cloud_keys)?)?,
                options,
            });
        } else {
            log.log(
                Level::Warning,
                SOURCE,
                &format!(
                    "Unable to disable SSH logins for {name} given \
                     ssh_redirect_user: {}. No cloud public-keys present.",
                    py_str(redirect)
                ),
            );
        }
    }

    Ok(steps)
}

/// The password state machine, transcribed rather than simplified.
///
/// It reads as four independent questions but is really one: *is there a
/// password worth unlocking the account for?* `lock_passwd` defaults to true,
/// so the account is disabled unless user-data says otherwise, and saying
/// otherwise is not enough on its own — there has to be a password too, either
/// already in `/etc/shadow` or in the user-data. The two `else` arms exist to
/// say out loud that `lock_passwd: false` was ignored, which is the kind of
/// silence that otherwise costs someone an afternoon.
fn password(
    name: &str,
    config: &Object,
    state: State,
    pre_existing: bool,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) {
    let mut has_existing_password = false;
    let mut ud_blank = false;
    let mut ud_specified = false;
    let mut password_key = "None";

    // Both keys are checked, so setting both runs `chpasswd` twice and the
    // hashed one wins.
    for (key, hashed) in [("plain_text_passwd", false), ("hashed_passwd", true)] {
        let Some(value) = config.get(key) else {
            continue;
        };
        ud_specified = true;
        password_key = key;
        if option::py_truthy(value) {
            steps.push(Step::SetPasswd {
                user: name.to_owned(),
                passwd: py_str(value),
                hashed,
            });
        } else {
            ud_blank = true;
        }
    }

    if pre_existing {
        if !ud_specified {
            if config.contains_key("passwd") {
                password_key = "passwd";
                log.log(
                    Level::Warning,
                    SOURCE,
                    &format!(
                        "'passwd' in user-data is ignored for existing user {name}"
                    ),
                );
            }
            has_existing_password = !state.shadow_password_is_empty;
        }
    } else if config.contains_key("passwd") {
        // For a new user `passwd:` already went to `useradd --password`, so
        // it counts as specified without a `chpasswd` of its own.
        ud_specified = true;
        password_key = "passwd";
        if !config.get("passwd").is_some_and(option::py_truthy) {
            ud_blank = true;
        }
    }

    if config.get("lock_passwd").is_none_or(option::py_truthy) {
        steps.push(Step::LockPasswd(name.to_owned()));
    } else if has_existing_password || ud_specified {
        if ud_blank {
            log.log(
                Level::Debug,
                SOURCE,
                &format!(
                    "Allowing unlocking empty password for {name} based on \
                     empty '{password_key}' in user-data"
                ),
            );
        }
        steps.push(Step::UnlockPasswd(name.to_owned()));
    } else if pre_existing {
        log.log(
            Level::Warning,
            SOURCE,
            &format!(
                "Not unlocking blank password for existing user {name}. \
                 'lock_passwd: false' present in user-data but no existing \
                 password set and no 'plain_text_passwd'/'hashed_passwd' \
                 provided in user-data"
            ),
        );
    } else {
        log.log(
            Level::Warning,
            SOURCE,
            &format!(
                "Not unlocking password for user {name}. 'lock_passwd: false' \
                 present in user-data but no 'passwd'/'plain_text_passwd'/\
                 'hashed_passwd' provided in user-data"
            ),
        );
    }
}

/// `add_snap_user`'s command.
fn snap_user(name: &str, config: &Object, log: &mut Logger) -> Step {
    let snapuser = config.get("snapuser").unwrap_or(&Value::Null);
    let mut argv: Vec<Value> = ["snap", "create-user", "--sudoer", "--json"]
        .into_iter()
        .map(|arg| Value::String(arg.to_owned()))
        .collect();
    if config.get("known").is_some_and(option::py_truthy) {
        argv.push(Value::String("--known".to_owned()));
    }
    if option::py_truthy(snapuser) {
        argv.push(snapuser.clone());
    } else {
        log.log(
            Level::Warning,
            SOURCE,
            &format!("invalid snap user: {}", py_str(snapuser)),
        );
    }
    log.log(Level::Debug, SOURCE, &format!("Adding snap user {name}"));
    Step::AddSnapUser { argv }
}

/// `type(x).__name__`, except that the message `str.replace` raises calls the
/// `None` singleton by its own name rather than its type's.
fn py_type(value: &Value) -> &'static str {
    if value.is_null() {
        "None"
    } else {
        type_name(value)
    }
}

/// `for x in value`, for the values a config can hold.
fn py_iter(value: &Value) -> Result<Vec<Value>, String> {
    match value {
        // A string iterates one character at a time, which is how a bare
        // `doas: "permit alice"` ends up validating the letter "p".
        Value::String(text) => {
            Ok(text.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Value::Array(items) => Ok(items.clone()),
        Value::Object(map) => {
            Ok(map.keys().map(|k| Value::String(k.clone())).collect())
        }
        other => Err(format!("'{}' object is not iterable", type_name(other))),
    }
}

/// `ssh_authorized_keys`, normalised the way `create_user` normalises it.
fn key_list(raw: &Value, log: &mut Logger) -> Result<Vec<String>, String> {
    let listed = match raw {
        Value::String(text) => vec![Value::String(text.clone())],
        Value::Object(map) => map.values().cloned().collect(),
        Value::Array(items) => items.clone(),
        // Upstream's `if keys is not None` guard skips the type check for a
        // null and then hands the null to `set()`, so this is a crash rather
        // than the empty list it looks like it should be. See B66.
        Value::Null => return Err("'NoneType' object is not iterable".to_owned()),
        other => {
            log.log(
                Level::Warning,
                SOURCE,
                &format!(
                    "Invalid type '<class '{}'>' detected for \
                     'ssh_authorized_keys', expected list, string, dict, or set.",
                    type_name(other)
                ),
            );
            Vec::new()
        }
    };
    key_set(&listed)
}

/// `set(keys)`, then sorted.
///
/// Upstream leaves it a set, whose iteration order is randomised per process,
/// so the order the keys land in `authorized_keys` differs from boot to boot.
/// Sorting is the only order that can be compared against it, and it is also
/// the better one to write.
fn key_set(listed: &[Value]) -> Result<Vec<String>, String> {
    let mut unique: Vec<&Value> = Vec::new();
    for key in listed {
        // `set()` refuses a list or a mapping before it ever gets written.
        if matches!(key, Value::Array(_) | Value::Object(_)) {
            let name = type_name(key);
            return Err(format!(
                "cannot use '{name}' as a set element (unhashable type: '{name}')"
            ));
        }
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    let mut keys: Vec<String> = unique.into_iter().map(py_str).collect();
    keys.sort();
    Ok(keys)
}

/// Carry out a [`plan`].
///
/// `root` is honoured for everything that is a file — the sudoers drop-in, the
/// doas file, the user's `authorized_keys`. It cannot be honoured for
/// `useradd`, `groupadd` or `chpasswd`, which write to the running system
/// whatever the argument list says, so those are refused outright unless
/// `root` is `/`. A test that reaches this function by accident stops here
/// instead of adding an account to the developer's machine.
pub fn run(steps: &[Step], root: &Path, log: &mut Logger) -> Result<(), String> {
    let live = root == Path::new("/");
    for step in steps {
        match step {
            Step::CreateGroup { name, argv } => groupadd(name, argv, root, live, log),
            Step::AddUser(add) => {
                log.log(
                    Level::Debug,
                    SOURCE,
                    &format!("Adding user {}", add.log_argv.join(" ")),
                );
                subp(&add.argv, None, live)?;
            }
            Step::AddSnapUser { argv } => {
                subp(&strings(argv)?, None, live)?;
            }
            Step::SetPasswd {
                user,
                passwd,
                hashed,
            } => {
                let mut argv = vec!["chpasswd".to_owned()];
                if *hashed {
                    // Short form: busybox and SLES 11 do not know `--encrypted`.
                    argv.push("-e".to_owned());
                }
                log.log(Level::Debug, SOURCE, &format!("chpasswd for {user}"));
                subp(&argv, Some(format!("{user}:{passwd}")), live).map_err(|err| {
                    format!("Failed to set password for {user}: {err}")
                })?;
            }
            Step::LockPasswd(user) => lock(user, live, log)?,
            Step::UnlockPasswd(user) => unlock(user, live, log)?,
            Step::WriteDoasRules { user, rules } => {
                if let Some(content) = user::doas_rules(user, rules, log)? {
                    drop_in(root, crate::DOAS_FN, &content)?;
                }
            }
            Step::WriteSudoRules { user, rules } => {
                let content = user::sudo_rules(user, rules)?;
                ensure_sudo_dir(root, log)?;
                drop_in(root, CI_SUDOERS_FN, &content)?;
            }
            Step::SetupUserKeys {
                user,
                keys,
                options,
            } => {
                ci_ssh::setup_user_keys(root, keys, user, options, log).map_err(
                    |err| format!("Failed to set up keys for {user}: {err}"),
                )?;
            }
        }
    }
    Ok(())
}

/// The `groupadd` half of `Distro.create_group`.
///
/// Never fails. Upstream logs and carries on here, unlike everywhere else: a
/// group that cannot be made is not fatal to the user, who simply does not get
/// it.
fn groupadd(name: &Value, argv: &[Value], root: &Path, live: bool, log: &mut Logger) {
    let name = py_str(name);
    if ci_sys::ids::gid_for_name(root, &name).is_some() {
        log.log(
            Level::Warning,
            SOURCE,
            &format!("Skipping creation of existing group '{name}'"),
        );
    } else if let Err(err) = strings(argv).and_then(|argv| subp(&argv, None, live)) {
        log.log(
            Level::Warning,
            SOURCE,
            &format!("Failed to create group {name}: {err}"),
        );
    } else {
        log.log(Level::Info, SOURCE, &format!("Created new group {name}"));
    }
}

/// `Distro.create_group`: the group, then the members it names.
///
/// `create_user` reaches the first half of this through [`Step::CreateGroup`];
/// only `cc_users_groups` asks for members, and only that half can fail the
/// boot — a `usermod` that errors propagates, while the `groupadd` above it
/// does not.
pub fn create_group(
    name: &Value,
    members: &[Value],
    root: &Path,
    snappy: bool,
    log: &mut Logger,
) -> Result<(), String> {
    let live = root == Path::new("/");
    groupadd(name, &user::groupadd(name, snappy), root, live, log);
    let group = py_str(name);
    for member in members {
        let member = py_str(member);
        if ci_sys::ids::passwd_entry(root, &member).is_none() {
            log.log(
                Level::Warning,
                SOURCE,
                &format!(
                    "Unable to add group member '{member}' to group '{group}'; \
                     user does not exist."
                ),
            );
            continue;
        }
        subp(
            &[
                "usermod".to_owned(),
                "-a".to_owned(),
                "-G".to_owned(),
                group.clone(),
                member.clone(),
            ],
            None,
            live,
        )?;
        log.log(
            Level::Info,
            SOURCE,
            &format!("Added user '{member}' to group '{group}'"),
        );
    }
    Ok(())
}

/// `Distro._shadow_file_has_empty_user_password`.
///
/// The one piece of [`State`] that has to be read off the machine rather than
/// asked of it: whether an account that already exists has a password field
/// that is blank or a bare `!`. That is what lets `lock_passwd: false` unlock
/// a pre-existing account without a password having been supplied — see the
/// five-flag chain in [`plan`].
pub fn shadow_password_is_empty(
    shadow_fn: &str,
    patterns: &[&str],
    root: &Path,
    username: &str,
    snappy: bool,
    log: &mut Logger,
) -> bool {
    let mut files = Vec::new();
    if snappy {
        files.push(crate::SHADOW_EXTRAUSERS_FN);
    }
    files.push(shadow_fn);

    for shadow_file in files {
        let path = root.join(shadow_file.trim_start_matches('/'));
        if !path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Only `^{username}:` is matched, so a name containing regex
        // metacharacters would be a pattern upstream too; the escape below is
        // the one deliberate divergence, and it can only ever match less.
        let quoted = regex::escape(username);
        if !matches_any(&content, &[format!("^{quoted}:")]) {
            log.log(
                Level::Debug,
                SOURCE,
                &format!("User {username} not found in {shadow_file}"),
            );
            continue;
        }
        log.log(
            Level::Debug,
            SOURCE,
            &format!(
                "User {username} found in {shadow_file}. Checking for empty password"
            ),
        );
        let empty: Vec<String> = patterns
            .iter()
            .map(|pattern| pattern.replace("{username}", &quoted))
            .collect();
        if matches_any(&content, &empty) {
            return true;
        }
    }
    false
}

/// `re.findall(..., re.MULTILINE)` reduced to "did anything match".
fn matches_any(content: &str, patterns: &[String]) -> bool {
    let joined = patterns.join("|");
    regex::RegexBuilder::new(&joined)
        .multi_line(true)
        .build()
        .is_ok_and(|re| re.is_match(content))
}

/// `passwd -l`, falling back to `usermod --lock`.
///
/// The short options are deliberate: SLES 11's `passwd` has no long form.
fn lock(user: &str, live: bool, log: &mut Logger) -> Result<(), String> {
    let argv = first_tool(&[&["passwd", "-l", user], &["usermod", "--lock", user]]).ok_or_else(
        || format!("Unable to lock user account '{user}'. No tools available.  Tried: [passwd, usermod]."),
    )?;
    log.log(
        Level::Debug,
        SOURCE,
        &format!("Locking password for {user}"),
    );
    subp(&argv, None, live)
        .map_err(|err| format!("Failed to disable password for user {user}: {err}"))
}

/// `passwd -u`, and then a blank password if that was refused.
///
/// `passwd -u` will not unlock an account whose password field is empty; it
/// says so on stdout and exits 0 anyway, which is why the output is what gets
/// tested rather than the status. Exit 3 is tolerated for the same reason.
fn unlock(user: &str, live: bool, log: &mut Logger) -> Result<(), String> {
    let argv = first_tool(&[&["passwd", "-u", user], &["usermod", "--unlock", user]]).ok_or_else(
        || format!("Unable to unlock user account '{user}'. No tools available.  Tried: [passwd, usermod]."),
    )?;
    log.log(
        Level::Debug,
        SOURCE,
        &format!("Unlocking password for {user}"),
    );
    let out = match ci_sys::subp::Subp::new(&argv).run() {
        Ok(out) if matches!(out.code, Some(0 | 3)) => out,
        Ok(out) => {
            return Err(format!(
                "Failed to enable password for user {user}: exit {:?}",
                out.code
            ))
        }
        Err(err) => {
            return Err(format!("Failed to enable password for user {user}: {err}"))
        }
    };
    if !live {
        return Ok(());
    }
    if out.stderr.is_empty() {
        return Ok(());
    }
    let argv = first_tool(&[
        &["passwd", "-d", user],
        &["usermod", "--password", "''", user],
    ])
    .ok_or_else(|| {
        format!("Unable to set blank password for user account '{user}'. No tools available.")
    })?;
    subp(&argv, None, live)
        .map_err(|err| format!("Failed to set blank password for user {user}: {err}"))
}

/// The first candidate whose program is on `PATH`.
fn first_tool(tools: &[&[&str]]) -> Option<Vec<String>> {
    tools
        .iter()
        .find(|tool| {
            tool.first()
                .is_some_and(|name| ci_sys::subp::which(name).is_some())
        })
        .map(|tool| tool.iter().map(|arg| (*arg).to_owned()).collect())
}

/// An argument list `subp` will accept, which is to say one with no numbers,
/// booleans or nulls left in it.
fn strings(argv: &[Value]) -> Result<Vec<String>, String> {
    argv.iter()
        .map(|arg| match arg {
            Value::String(text) => Ok(text.clone()),
            other => Err(format!(
                "expected str, bytes or os.PathLike object, not {}",
                type_name(other)
            )),
        })
        .collect()
}

/// Run a command, or refuse to when the caller is not operating on `/`.
fn subp(argv: &[String], stdin: Option<String>, live: bool) -> Result<(), String> {
    if !live {
        return Err(format!(
            "refusing to run {:?} against a rooted tree",
            argv.first().map_or("", String::as_str)
        ));
    }
    let mut cmd = ci_sys::subp::Subp::new(argv);
    if let Some(data) = stdin {
        cmd = cmd.stdin(data);
    }
    cmd.check().map(|_| ()).map_err(|err| err.to_string())
}

/// `util.write_file` for a new drop-in, `util.append_file` for an existing one
/// that does not already contain the block.
///
/// The containment test is upstream's and is why a second boot does not
/// duplicate the rules — and also why editing a rule by hand leaves the old
/// one in place and appends the new.
fn drop_in(root: &Path, logical: &str, content: &str) -> Result<(), String> {
    let path = root.join(logical.trim_start_matches('/'));
    let existing = std::fs::read_to_string(&path);
    match existing {
        Ok(text) if text.contains(content) => Ok(()),
        Ok(_) => ci_sys::atomic::append_file(&path, content, 0o440)
            .map_err(|err| format!("Failed to append to {logical}: {err}")),
        Err(_) => {
            if let Some(parent) = path.parent() {
                ci_sys::path::ensure_dir(parent, 0o750)
                    .map_err(|err| format!("Failed to create {logical}: {err}"))?;
            }
            let body = format!(
                "{}\n{content}",
                ci_core::version::make_header('#', "created")
            );
            ci_sys::atomic::write_file(
                &path,
                body,
                ci_sys::atomic::WriteOptions::mode(0o440),
            )
            .map_err(|err| format!("Failed to write {logical}: {err}"))
        }
    }
}

/// Make sure `/etc/sudoers` includes the drop-in directory, then make the
/// directory.
///
/// Without the `#includedir` line the drop-in is inert, so this runs before
/// every sudoers write rather than once. The scan accepts both `#includedir`
/// and `@includedir`, and compares absolute paths, so a relative include of
/// the same directory counts.
fn ensure_sudo_dir(root: &Path, log: &mut Logger) -> Result<(), String> {
    let dir = Path::new(CI_SUDOERS_FN)
        .parent()
        .map_or_else(|| "/etc/sudoers.d".to_owned(), |p| p.display().to_string());
    let base = root.join(SUDO_BASE.trim_start_matches('/'));
    let system = root.join(SYSTEM_SUDO_BASE.trim_start_matches('/'));

    let mut contents = String::new();
    let mut base_exists = false;
    if let Ok(text) = std::fs::read_to_string(&base) {
        contents = text;
        base_exists = true;
    } else if let Ok(text) = std::fs::read_to_string(&system) {
        contents = text;
    }

    let found = contents.lines().any(|line| {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('#').or_else(|| line.strip_prefix('@'))
        else {
            return false;
        };
        let Some(included) = rest.strip_prefix("includedir") else {
            return false;
        };
        let included = included.trim();
        // `os.path.abspath` on a relative include resolves against the
        // process's cwd, which is not something worth reproducing; the
        // comparison it feeds only ever matches an absolute path in practice.
        !included.is_empty() && included == dir
    });

    if !found {
        let added = ci_core::version::make_header('#', "added");
        let body = if base_exists {
            format!("\n{added}\n#includedir {dir}\n")
        } else {
            if !contents.is_empty() {
                log.log(
                    Level::Info,
                    SOURCE,
                    &format!("Using content from '{SYSTEM_SUDO_BASE}'"),
                );
            }
            format!(
                "# See sudoers(5) for more information on \"#include\" directives:\n\n\
                 {added}\n#includedir {dir}\n"
            )
        };
        if base_exists {
            ci_sys::atomic::append_file(&base, &body, 0o440)
        } else {
            if let Some(parent) = base.parent() {
                ci_sys::path::ensure_dir(parent, 0o755)
                    .map_err(|err| format!("Failed to write {SUDO_BASE}: {err}"))?;
            }
            ci_sys::atomic::write_file(
                &base,
                format!("{contents}{body}"),
                ci_sys::atomic::WriteOptions::mode(0o440),
            )
        }
        .map_err(|err| format!("Failed to write {SUDO_BASE}: {err}"))?;
        log.log(
            Level::Debug,
            SOURCE,
            &format!("Added '#includedir {dir}' to {SUDO_BASE}"),
        );
    }

    ci_sys::path::ensure_dir(root.join(dir.trim_start_matches('/')), 0o750)
        .map_err(|err| format!("Failed to create sudoers dir: {err}"))
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

    fn cfg(json: &str) -> Object {
        match serde_json::from_str(json).unwrap() {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn names(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                Step::CreateGroup { name, .. } => format!("group:{}", py_str(name)),
                Step::AddUser(_) => "useradd".to_owned(),
                Step::AddSnapUser { .. } => "snap".to_owned(),
                Step::SetPasswd { hashed, .. } => format!("passwd:hashed={hashed}"),
                Step::LockPasswd(_) => "lock".to_owned(),
                Step::UnlockPasswd(_) => "unlock".to_owned(),
                Step::WriteDoasRules { .. } => "doas".to_owned(),
                Step::WriteSudoRules { .. } => "sudo".to_owned(),
                Step::SetupUserKeys { keys, .. } => format!("keys:{}", keys.join("|")),
            })
            .collect()
    }

    fn plan_of(json: &str, state: State) -> Vec<Step> {
        let mut log = Logger::silent();
        plan("alice", &cfg(json), state, &mut log).unwrap()
    }

    #[test]
    fn an_account_with_nothing_said_about_it_is_locked() {
        assert_eq!(names(&plan_of("{}", State::default())), ["useradd", "lock"]);
    }

    #[test]
    fn the_password_is_set_before_the_lock_not_after() {
        assert_eq!(
            names(&plan_of(
                r#"{"plain_text_passwd": "hunter2"}"#,
                State::default()
            )),
            ["useradd", "passwd:hashed=false", "lock"]
        );
    }

    #[test]
    fn unlocking_needs_a_password_as_well_as_permission() {
        assert_eq!(
            names(&plan_of(r#"{"lock_passwd": false}"#, State::default())),
            ["useradd"]
        );
        assert_eq!(
            names(&plan_of(
                r#"{"lock_passwd": false, "passwd": "$6$x"}"#,
                State::default()
            )),
            ["useradd", "unlock"]
        );
    }

    #[test]
    fn an_existing_user_may_be_unlocked_on_the_strength_of_its_shadow_entry() {
        let existing = State {
            user_exists: true,
            shadow_password_is_empty: false,
            snappy: false,
        };
        assert_eq!(
            names(&plan_of(r#"{"lock_passwd": false}"#, existing)),
            ["unlock"]
        );
        let blank = State {
            shadow_password_is_empty: true,
            ..existing
        };
        assert!(names(&plan_of(r#"{"lock_passwd": false}"#, blank)).is_empty());
    }

    #[test]
    fn passwd_is_ignored_for_an_existing_user_but_plain_text_is_not() {
        let existing = State {
            user_exists: true,
            shadow_password_is_empty: true,
            snappy: false,
        };
        assert_eq!(names(&plan_of(r#"{"passwd": "$6$x"}"#, existing)), ["lock"]);
        assert_eq!(
            names(&plan_of(r#"{"plain_text_passwd": "s"}"#, existing)),
            ["passwd:hashed=false", "lock"]
        );
    }

    #[test]
    fn an_existing_user_gets_no_groups_and_no_useradd() {
        let existing = State {
            user_exists: true,
            ..State::default()
        };
        assert_eq!(
            names(&plan_of(r#"{"groups": "sudo,adm"}"#, existing)),
            ["lock"]
        );
    }

    #[test]
    fn groups_are_created_before_the_user_that_joins_them() {
        assert_eq!(
            names(&plan_of(r#"{"groups": "sudo,adm"}"#, State::default())),
            ["group:sudo", "group:adm", "useradd", "lock"]
        );
    }

    #[test]
    fn keys_are_deduplicated_and_ordered() {
        assert_eq!(
            names(&plan_of(
                r#"{"ssh_authorized_keys": ["b", "a", "b"]}"#,
                State::default()
            )),
            ["useradd", "lock", "keys:a|b"]
        );
    }

    #[test]
    fn a_single_key_string_is_not_split_into_characters() {
        assert_eq!(
            names(&plan_of(
                r#"{"ssh_authorized_keys": "ssh-rsa AAA a"}"#,
                State::default()
            )),
            ["useradd", "lock", "keys:ssh-rsa AAA a"]
        );
    }

    #[test]
    fn a_null_key_list_aborts_the_user() {
        let mut log = Logger::silent();
        let err = plan(
            "alice",
            &cfg(r#"{"ssh_authorized_keys": null}"#),
            State::default(),
            &mut log,
        )
        .unwrap_err();
        assert!(err.contains("NoneType"), "{err}");
    }

    #[test]
    fn a_redirect_without_cloud_keys_installs_nothing() {
        assert_eq!(
            names(&plan_of(
                r#"{"ssh_redirect_user": "ubuntu"}"#,
                State::default()
            )),
            ["useradd", "lock"]
        );
    }

    #[test]
    fn a_redirect_names_both_users_in_the_forced_command() {
        let steps = plan_of(
            r#"{"ssh_redirect_user": "ubuntu", "cloud_public_ssh_keys": ["k"]}"#,
            State::default(),
        );
        let Some(Step::SetupUserKeys { options, .. }) = steps.last() else {
            panic!("no key step: {steps:?}");
        };
        assert!(options.contains(r#"\"ubuntu\""#), "{options}");
        assert!(options.contains(r#"\"alice\""#), "{options}");
    }

    #[test]
    fn a_snap_user_replaces_everything_else() {
        assert_eq!(
            names(&plan_of(
                r#"{"snapuser": "me@example.com", "sudo": "ALL=(ALL) ALL"}"#,
                State::default()
            )),
            ["snap"]
        );
    }

    #[test]
    fn a_bare_doas_string_is_iterated_by_character() {
        let steps = plan_of(r#"{"doas": "permit alice"}"#, State::default());
        let Some(Step::WriteDoasRules { rules, .. }) = steps
            .iter()
            .find(|s| matches!(s, Step::WriteDoasRules { .. }))
        else {
            panic!("no doas step: {steps:?}");
        };
        assert_eq!(rules.len(), "permit alice".len());
    }

    #[test]
    fn nothing_runs_against_a_rooted_tree() {
        let mut log = Logger::silent();
        let steps = vec![Step::LockPasswd("alice".to_owned())];
        let err = run(&steps, Path::new("/nonexistent-root"), &mut log).unwrap_err();
        assert!(err.contains("rooted"), "{err}");
    }
}
