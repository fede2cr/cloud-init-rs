//! Port of `cc_set_passwords.py` — the other way into the machine.
//!
//! `cc_ssh` installs the keys; this module sets the passwords and decides
//! whether `sshd` will accept one at all. The two halves are independent, and
//! the second runs even when the first has failed. That is the single most
//! surprising thing about the module: every `chpasswd` and every `passwd
//! --expire` error is collected rather than raised, `PasswordAuthentication`
//! is written regardless, and only then is the *last* collected error
//! re-raised. Getting the order wrong leaves either a box nobody can log into
//! or one anybody can.
//!
//! Split the same way as the rest of the port: [`plan`] turns config plus a
//! handful of host facts into an ordered list of [`Step`]s, [`run`] carries
//! them out. Everything [`State`] holds is readable before anything is
//! written — including whether `sshd_config` would change, which is a property
//! of the file on disk rather than of the write.

use std::path::Path;

use ci_config::{option, Object, Value};
use ci_distro::ug;
use ci_log::{Level, Logger};

use super::{py_str, Args};

const SOURCE: &str = "cc_set_passwords.py";

/// `features.EXPIRE_APPLIES_TO_HASHED_USERS`.
const EXPIRE_APPLIES_TO_HASHED_USERS: bool = true;

/// The generated-password alphabet: `string.digits`, `ascii_lowercase`,
/// `ascii_uppercase` and `punctuation`, one character taken from each.
const ALPHABET: [&[u8]; 4] = [
    b"0123456789",
    b"abcdefghijklmnopqrstuvwxyz",
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    b"!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
];

/// `rand_user_password`'s only caller never passes a length.
const PASSWORD_LEN: usize = 20;

/// What the module reads off the machine before it changes anything.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// `distro.get_option("ssh_svcname", "ssh")`.
    pub ssh_svcname: String,
    /// `distro.uses_systemd()`.
    pub systemd: bool,
    /// Whether writing `PasswordAuthentication` would change `sshd_config` —
    /// the value `update_ssh_config` is about to return. It is settled up
    /// front because it is a fact about the file as it stands, and because the
    /// restart below hangs on it.
    pub sshd_config_changes: bool,
    /// `systemctl show --property ActiveState --value <svc>`, stripped. Only
    /// consulted under systemd.
    pub ssh_active_state: String,
}

/// One upstream call, in the order `handle` makes them.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `distro.chpasswd(plist, hashed)`.
    ///
    /// The pairs are values rather than strings because upstream joins them
    /// with `":"`, and a non-string on either side raises there — inside the
    /// `try`. The failure belongs to the step, not to the plan.
    Chpasswd {
        entries: Vec<(Value, Value)>,
        hashed: bool,
    },
    /// The `multi_log` of every password this run invented.
    AnnounceRandom(String),
    /// `distro.expire_passwd(user)`.
    ExpirePasswd(Value),
    /// `update_ssh_config({"PasswordAuthentication": value})`.
    UpdateSshConfig { value: String },
    /// `distro.manage_service("restart", service, ...)`.
    RestartSsh {
        service: String,
        ignore_dependencies: bool,
    },
    /// An exception `handle_ssh_pwauth` raises rather than logs.
    ///
    /// A step and not an `Err` because of where it happens: after the
    /// passwords are set and after the expiries, so everything above it must
    /// still run. Escaping uncaught, it also discards the errors collected on
    /// the way.
    Abort(String),
}

/// `cc_set_passwords.handle`, decided but not done.
///
/// `rand` stands in for `rand_user_password` so that a caller needing a
/// reproducible plan can supply its own.
///
/// The `Err` case is an exception raised before any step could be taken — a
/// `chpasswd:` that is not a mapping, an entry with no `name`. Nothing has
/// happened yet when one of those comes back.
pub fn plan(
    cfg: &Object,
    state: &State,
    args: &Value,
    default_user: Option<&Value>,
    rand: &mut dyn FnMut() -> String,
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    // Two sources for a single password, and the command line wins by deleting
    // the config's list outright.
    let mut cfg = cfg.clone();
    let password = match args.as_array().and_then(|list| list.first()) {
        Some(first) => {
            command_line_wins(&mut cfg)?;
            Some(py_str(first))
        }
        None => cfg.get("password").map(py_str),
    };

    let mut expire = true;
    let mut plist: Vec<Value> = Vec::new();
    let mut users_list: Vec<Value> = Vec::new();

    // `chpasswd:` is read with `in` and `[]` rather than `.get`, so a string
    // or a list here does not fail on sight: it fails only if one of the three
    // keys happens to be a substring of it, or an element of it.
    if let Some(chfg) = cfg.get("chpasswd") {
        users_list = if member(chfg, "users")? {
            as_list(index(chfg, "users")?)
        } else {
            Vec::new()
        };

        if member(chfg, "list")? {
            let list = index(chfg, "list")?;
            if option::py_truthy(list) {
                deprecate(
                    log,
                    "Config key 'lists' is deprecated in 22.3 and scheduled to \
                     be removed in 27.3. Use 'users' instead.",
                );
                if list.is_array() {
                    log.log(
                        Level::Debug,
                        SOURCE,
                        "Handling input for chpasswd as list.",
                    );
                    plist = as_list(list);
                } else {
                    deprecate(
                        log,
                        "The chpasswd multiline string is deprecated in 22.2 \
                         and scheduled to be removed in 27.2. Use string type \
                         instead.",
                    );
                    log.log(
                        Level::Debug,
                        SOURCE,
                        "Handling input for chpasswd as multiline string.",
                    );
                    plist = ci_core::pystr::split_lines(&py_str(list))
                        .into_iter()
                        .map(|line| Value::String(line.to_owned()))
                        .collect();
                }
            }
        }

        if member(chfg, "expire")? {
            expire = option::translate_bool(index(chfg, "expire")?);
        }
    }

    // A bare `password:` only reaches anyone if neither list named a user.
    if users_list.is_empty() && plist.is_empty() {
        if let Some(password) = password.filter(|text| !text.is_empty()) {
            let ug::Normalized { mut users, .. } =
                ug::normalize_users_groups(&cfg, default_user, log)?;
            if let Some((user, _)) = ug::extract_default(&mut users) {
                plist = vec![Value::String(format!("{user}:{password}"))];
            } else {
                log.warning(
                    SOURCE,
                    "No default or defined user to change password for.",
                );
            }
        }
    }

    let mut steps = Vec::new();
    if !plist.is_empty() || !users_list.is_empty() {
        steps = passwords(&plist, &users_list, expire, rand, log)?;
    }
    steps.extend(ssh_pwauth(cfg.get("ssh_pwauth"), state, log));
    Ok(steps)
}

/// `del cfg["chpasswd"]["list"]`, and the two ways it can fail.
fn command_line_wins(cfg: &mut Object) -> Result<(), String> {
    let Some(chpasswd) = cfg.get_mut("chpasswd") else {
        return Ok(());
    };
    // `"list" in <str>` is a substring test and `in` on a list is membership.
    // Either can be true, and it is the `del` that then fails.
    if !member(chpasswd, "list")? {
        return Ok(());
    }
    match chpasswd {
        Value::Object(map) => {
            map.shift_remove("list");
            Ok(())
        }
        Value::Array(_) => {
            Err("list indices must be integers or slices, not str".to_owned())
        }
        other => Err(format!(
            "'{}' object does not support item deletion",
            ci_config::type_name(other)
        )),
    }
}

/// Everything between `if plist or users_list:` and `handle_ssh_pwauth`.
fn passwords(
    plist: &[Value],
    users_list: &[Value],
    expire: bool,
    rand: &mut dyn FnMut() -> String,
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let mut plain = users_by_type(users_list, "text")?;
    let mut users: Vec<Value> = plain.iter().map(|(name, _)| name.clone()).collect();
    let mut hashed = users_by_type(users_list, "hash")?;
    let mut hashed_users: Vec<Value> =
        hashed.iter().map(|(name, _)| name.clone()).collect();
    let mut randlist: Vec<String> = Vec::new();

    for (name, _) in users_by_type(users_list, "RANDOM")? {
        let secret = rand();
        randlist.push(format!("{}:{secret}", py_str(&name)));
        users.push(name.clone());
        plain.push((name, Value::String(secret)));
    }

    // The deprecated `chpasswd: list:` form, one `user:password` line each.
    for line in plist {
        let (user, mut secret) = split_once(line)?;
        if is_hash(&secret) {
            hashed_users.push(Value::String(user.clone()));
            hashed.push((Value::String(user), Value::String(secret)));
        } else {
            if secret == "R" || secret == "RANDOM" {
                secret = rand();
                randlist.push(format!("{user}:{secret}"));
            }
            users.push(Value::String(user.clone()));
            plain.push((Value::String(user), Value::String(secret)));
        }
    }

    let mut steps = Vec::new();
    if !users.is_empty() {
        log.log(
            Level::Debug,
            SOURCE,
            &format!("Changing password for {}:", repr_list(&users)),
        );
        steps.push(Step::Chpasswd {
            entries: plain,
            hashed: false,
        });
    }
    if !hashed_users.is_empty() {
        log.log(
            Level::Debug,
            SOURCE,
            &format!("Setting hashed password for {}:", repr_list(&hashed_users)),
        );
        steps.push(Step::Chpasswd {
            entries: hashed,
            hashed: true,
        });
    }
    if !randlist.is_empty() {
        steps.push(Step::AnnounceRandom(format!(
            "Set the following 'random' passwords\n\n{}\n",
            randlist.join("\n")
        )));
    }

    if expire {
        if EXPIRE_APPLIES_TO_HASHED_USERS {
            users.extend(hashed_users);
        }
        steps.extend(users.into_iter().map(Step::ExpirePasswd));
    }
    Ok(steps)
}

/// `get_users_by_type`: `name` is required, `type` defaults to `hash`.
fn users_by_type(
    users_list: &[Value],
    pw_type: &str,
) -> Result<Vec<(Value, Value)>, String> {
    let mut out = Vec::new();
    for item in users_list {
        let map = item.as_object().ok_or_else(|| no_attribute(item, "get"))?;
        // A non-string `type` cannot equal any of the three literals, so it
        // drops the entry rather than raising.
        let kind = map
            .get("type")
            .map_or("hash", |kind| kind.as_str().unwrap_or("\0"));
        if kind != pw_type {
            continue;
        }
        // `str(KeyError("name"))` is the repr of the key alone, not a
        // sentence: there is no `KeyError: ` prefix to reproduce.
        let name = map.get("name").ok_or_else(|| "'name'".to_owned())?;
        let password = map
            .get("password")
            .cloned()
            .unwrap_or_else(|| Value::String("RANDOM".to_owned()));
        out.push((name.clone(), password));
    }
    Ok(out)
}

/// `handle_ssh_pwauth`.
fn ssh_pwauth(pw_auth: Option<&Value>, state: &State, log: &mut Logger) -> Vec<Step> {
    const CFG_NAME: &str = "PasswordAuthentication";

    if pw_auth.is_some_and(Value::is_string) {
        deprecate(
            log,
            "Using a string value for the 'ssh_pwauth' key is deprecated in \
             22.2 and scheduled to be removed in 27.2. Use a boolean value \
             with 'ssh_pwauth'.",
        );
    }

    let value = if pw_auth.is_some_and(option::is_true) {
        "yes"
    } else if pw_auth.is_some_and(option::is_false) {
        "no"
    } else {
        let bmsg = format!("Leaving SSH config '{CFG_NAME}' unchanged.");
        match pw_auth {
            // An explicit `ssh_pwauth: null` is the same `None` an absent key
            // gives, and takes the same branch rather than the `.lower()` one.
            None | Some(Value::Null) => {
                log.log(Level::Debug, SOURCE, &format!("{bmsg} ssh_pwauth=None"));
            }
            Some(Value::String(text)) if text.to_lowercase() == "unchanged" => {
                log.log(Level::Debug, SOURCE, &format!("{bmsg} ssh_pwauth={text}"));
            }
            Some(Value::String(text)) => log.warning(
                SOURCE,
                &format!("{bmsg} Unrecognized value: ssh_pwauth={text}"),
            ),
            // Upstream reaches for `.lower()` on whatever is left, which is
            // everything neither a string nor recognisably a boolean. See the
            // upstream bug filed against this in docs/COMPAT.md.
            Some(other) => return vec![Step::Abort(no_attribute(other, "lower"))],
        }
        return Vec::new();
    };

    let mut steps = vec![Step::UpdateSshConfig {
        value: value.to_owned(),
    }];
    if !state.sshd_config_changes {
        log.log(
            Level::Debug,
            SOURCE,
            &format!("No need to restart SSH service, {CFG_NAME} not updated."),
        );
        return steps;
    }

    let service = state.ssh_svcname.clone();
    if state.systemd {
        // Reachable only when someone started the network stage by hand.
        // Restarting with dependencies honoured would deadlock against this
        // module's own `Before=sshd.service`.
        if ["active", "activating", "reloading"]
            .contains(&state.ssh_active_state.to_lowercase().as_str())
        {
            steps.push(Step::RestartSsh {
                service,
                ignore_dependencies: true,
            });
        }
    } else {
        steps.push(Step::RestartSsh {
            service,
            ignore_dependencies: false,
        });
    }
    steps
}

/// The port's `rand_user_password`: one character from each class, the rest
/// from all four, then shuffled.
///
/// Drawn entirely from the kernel. Upstream takes sixteen of the twenty
/// characters from `random.SystemRandom`, but the other four — and the shuffle
/// that hides where they sit — from the global Mersenne Twister.
pub fn rand_password() -> Result<String, String> {
    let all: Vec<u8> = ALPHABET.concat();
    let mut draw = Draw::new(PASSWORD_LEN * 2)?;

    let mut chars: Vec<u8> = Vec::with_capacity(PASSWORD_LEN);
    for set in ALPHABET {
        chars.push(draw.pick(set)?);
    }
    while chars.len() < PASSWORD_LEN {
        chars.push(draw.pick(&all)?);
    }
    // Fisher-Yates: without it the four class picks are always first.
    for index in (1..chars.len()).rev() {
        let target = draw.below(index + 1)?;
        chars.swap(index, target);
    }
    String::from_utf8(chars).map_err(|err| err.to_string())
}

/// A fixed supply of kernel randomness, spent one draw at a time.
struct Draw {
    words: std::vec::IntoIter<u64>,
}

impl Draw {
    fn new(count: usize) -> Result<Self, String> {
        let mut bytes = vec![0u8; count * 8];
        ci_sys::rand::fill(&mut bytes).map_err(|err| {
            format!("Failed to read random bytes for a password: {err}")
        })?;
        let words: Vec<u64> = bytes
            .chunks_exact(8)
            .map(|chunk| {
                let mut word = [0u8; 8];
                word.copy_from_slice(chunk);
                u64::from_ne_bytes(word)
            })
            .collect();
        Ok(Self {
            words: words.into_iter(),
        })
    }

    fn below(&mut self, bound: usize) -> Result<usize, String> {
        let word = self.words.next().ok_or("ran out of random bytes")?;
        usize::try_from(word % bound as u64).map_err(|err| err.to_string())
    }

    fn pick(&mut self, set: &[u8]) -> Result<u8, String> {
        let index = self.below(set.len())?;
        set.get(index)
            .copied()
            .ok_or_else(|| "empty set".to_owned())
    }
}

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let service = args
        .system_info
        .get("ssh_svcname")
        .map_or_else(|| args.distro.ssh_svcname.to_owned(), py_str);
    // Only the two settled answers reach `sshd_config`, and resolving which
    // file to probe can create a drop-in, so do not ask about the third.
    let pw_auth = args.cfg.get("ssh_pwauth");
    let value = if pw_auth.is_some_and(option::is_true) {
        Some("yes")
    } else if pw_auth.is_some_and(option::is_false) {
        Some("no")
    } else {
        None
    };
    let sshd_config_changes = value.is_some_and(|value| {
        ci_ssh::would_update_ssh_config(
            args.root,
            &[("PasswordAuthentication", value)],
            ci_ssh::DEF_SSHD_CFG,
            args.logger,
        )
        // Unreadable is not unchanged: assume the write will land, so that a
        // restart still happens and the setting takes effect.
        .unwrap_or(true)
    });
    let state = State {
        systemd: uses_systemd(args.root),
        sshd_config_changes,
        ssh_active_state: active_state(&service, args.root),
        ssh_svcname: service,
    };
    let default_user = args.system_info.get("default_user").cloned();
    // A password that could not be drawn must not become the empty string, so
    // the failure is carried out of the closure rather than swallowed.
    let mut failed: Option<String> = None;
    let steps = plan(
        args.cfg,
        &state,
        args.args,
        default_user.as_ref(),
        &mut || match rand_password() {
            Ok(secret) => secret,
            Err(err) => {
                failed.get_or_insert(err);
                String::new()
            }
        },
        args.logger,
    )?;
    if let Some(err) = failed {
        return Err(err);
    }
    run(&steps, args.root, args.logger)
}

/// Carry the plan out, collecting what upstream collects.
pub fn run(steps: &[Step], root: &Path, log: &mut Logger) -> Result<(), String> {
    // `chpasswd` and `passwd` write to the running system whatever their
    // arguments say, so a rooted run must not reach them.
    let live = root == Path::new("/");
    let mut errors: Vec<String> = Vec::new();
    let mut expired: Vec<Value> = Vec::new();

    for step in steps {
        match step {
            Step::Chpasswd { entries, hashed } => {
                if let Err(err) = chpasswd(entries, *hashed, live) {
                    let names: Vec<Value> =
                        entries.iter().map(|(name, _)| name.clone()).collect();
                    let what = if *hashed {
                        "hashed passwords"
                    } else {
                        "passwords"
                    };
                    log.warning(
                        SOURCE,
                        &format!(
                            "Failed to set {what} with chpasswd for {}: {err}",
                            repr_list(&names)
                        ),
                    );
                    errors.push(err);
                }
            }
            Step::AnnounceRandom(text) => multi_log(text),
            Step::ExpirePasswd(user) => match expire_passwd(user, live) {
                Ok(()) => expired.push(user.clone()),
                Err(err) => {
                    log.warning(
                        SOURCE,
                        &format!(
                            "Failed to set 'expire' for {}: {err}",
                            ci_config::repr(user)
                        ),
                    );
                    errors.push(err);
                }
            },
            Step::UpdateSshConfig { value } => {
                ci_ssh::update_ssh_config(
                    root,
                    &[("PasswordAuthentication", value.as_str())],
                    ci_ssh::DEF_SSHD_CFG,
                    log,
                )
                .map_err(|err| err.to_string())?;
            }
            Step::RestartSsh {
                service,
                ignore_dependencies,
            } => restart(service, *ignore_dependencies, live, log),
            Step::Abort(message) => return Err(message.clone()),
        }
    }

    if !expired.is_empty() {
        log.log(
            Level::Debug,
            SOURCE,
            &format!("Expired passwords for: {} users", repr_list(&expired)),
        );
    }
    match errors.pop() {
        Some(last) => {
            log.log(
                Level::Debug,
                SOURCE,
                &format!(
                    "{} errors occurred, re-raising the last one",
                    errors.len() + 1
                ),
            );
            Err(last)
        }
        None => Ok(()),
    }
}

/// `Distro.chpasswd`: one command, every pair on its stdin.
fn chpasswd(
    entries: &[(Value, Value)],
    hashed: bool,
    live: bool,
) -> Result<(), String> {
    let mut payload = String::new();
    for (name, password) in entries {
        // `":".join([name, password])`, which is where a non-string dies.
        for (index, part) in [name, password].into_iter().enumerate() {
            let text = part.as_str().ok_or_else(|| {
                format!(
                    "sequence item {index}: expected str instance, {} found",
                    ci_config::type_name(part)
                )
            })?;
            if index == 1 {
                payload.push(':');
            }
            payload.push_str(text);
        }
        payload.push('\n');
    }
    let mut argv = vec!["chpasswd".to_owned()];
    if hashed {
        // Short form: busybox and SLES 11 do not know `--encrypted`.
        argv.push("-e".to_owned());
    }
    subp(&argv, Some(payload), live)
}

/// `Distro.expire_passwd`.
fn expire_passwd(user: &Value, live: bool) -> Result<(), String> {
    let Some(name) = user.as_str() else {
        // `subp` will not encode a non-string argv element, and says so with
        // its own report rather than a `TypeError`.
        let argv = repr_list(&[
            Value::String("passwd".to_owned()),
            Value::String("--expire".to_owned()),
            user.clone(),
        ]);
        return Err(format!(
            "Unexpected error while running command.\n\
             Command: {argv}\n\
             Exit code: -\n\
             Reason: Running invalid command: {argv}\n\
             Stdout: -\n\
             Stderr: -"
        ));
    };
    subp(
        &["passwd".to_owned(), "--expire".to_owned(), name.to_owned()],
        None,
        live,
    )
}

fn restart(service: &str, ignore_dependencies: bool, live: bool, log: &mut Logger) {
    let mut argv = vec![
        "systemctl".to_owned(),
        "restart".to_owned(),
        service.to_owned(),
    ];
    if ignore_dependencies {
        argv.push("--job-mode=ignore-dependencies".to_owned());
    }
    match subp(&argv, None, live) {
        Ok(()) => log.log(Level::Debug, SOURCE, "Restarted the SSH daemon."),
        Err(err) => log.warning(
            SOURCE,
            &format!(
                "'ssh_pwauth' configuration may not be applied. Cloud-init was \
                 unable to restart SSH daemon due to error: '{err}'"
            ),
        ),
    }
}

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

/// `log_util.multi_log(text, stderr=False, fallback_to_stdout=False)`, whose
/// only remaining sink is the console.
fn multi_log(text: &str) {
    super::multi_log_console(text);
}

/// `distros.uses_systemd`: `/run/systemd/system` is a directory, not followed.
fn uses_systemd(root: &Path) -> bool {
    super::uses_systemd(root)
}

fn active_state(service: &str, root: &Path) -> String {
    if root != Path::new("/") {
        return String::new();
    }
    ci_sys::subp::Subp::new([
        "systemctl",
        "show",
        "--property",
        "ActiveState",
        "--value",
        service,
    ])
    .check()
    .map(|out| out.stdout_lossy().trim().to_owned())
    .unwrap_or_default()
}

/// `lifecycle.deprecate`, which logs against its own module.
fn deprecate(log: &mut Logger, message: &str) {
    log.log(Level::Deprecated, "lifecycle.py", message);
}

/// A list as `%s` renders it: its `repr`.
fn repr_list(items: &[Value]) -> String {
    ci_config::repr(&Value::Array(items.to_vec()))
}

/// The tail of `util.get_cfg_option_list` once the key is known to be there:
/// `null` is empty, and a non-list is a one-element list of its `str()`.
fn as_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.clone(),
        other => vec![Value::String(py_str(other))],
    }
}

/// `key in container`.
fn member(haystack: &Value, key: &str) -> Result<bool, String> {
    match haystack {
        Value::Object(map) => Ok(map.contains_key(key)),
        Value::String(text) => Ok(text.contains(key)),
        Value::Array(items) => Ok(items.iter().any(|item| item == key)),
        other => Err(not_a_container(other)),
    }
}

/// `container[key]`, reached only once [`member`] has said the key is there.
fn index<'a>(haystack: &'a Value, key: &str) -> Result<&'a Value, String> {
    match haystack {
        Value::Object(map) => map.get(key).ok_or_else(|| ci_config::repr_str(key)),
        Value::String(_) => {
            Err("string indices must be integers, not 'str'".to_owned())
        }
        other => Err(format!(
            "{} indices must be integers or slices, not str",
            ci_config::type_name(other)
        )),
    }
}

/// `u, p = line.split(":", 1)`, which needs exactly two parts.
fn split_once(line: &Value) -> Result<(String, String), String> {
    let text = line.as_str().ok_or_else(|| no_attribute(line, "split"))?;
    text.split_once(':')
        .map(|(user, secret)| (user.to_owned(), secret.to_owned()))
        .ok_or_else(|| "not enough values to unpack (expected 2, got 1)".to_owned())
}

/// `re.match(r"\$(1|2a|2y|5|6)(\$.+){2}", p) is not None and ":" not in p`.
///
/// Hand-rolled rather than a regex: `(\$.+){2}` only needs a second `$` with
/// something after it, on the same line, because `.` does not match a newline.
fn is_hash(secret: &str) -> bool {
    if secret.contains(':') {
        return false;
    }
    let Some(rest) = secret.strip_prefix('$') else {
        return false;
    };
    let Some(rest) = ["2a", "2y", "1", "5", "6"]
        .into_iter()
        .find_map(|id| rest.strip_prefix(id))
    else {
        return false;
    };
    let line = rest.split('\n').next().unwrap_or_default();
    // `\$.+` twice over: a `$` at the front, then a later `$` that is neither
    // adjacent to it nor last.
    line.starts_with('$')
        && line
            .char_indices()
            .any(|(at, ch)| ch == '$' && at >= 2 && at + ch.len_utf8() < line.len())
}

fn not_a_container(value: &Value) -> String {
    format!(
        "argument of type '{}' is not a container or iterable",
        ci_config::type_name(value)
    )
}

fn no_attribute(value: &Value, attr: &str) -> String {
    format!(
        "'{}' object has no attribute '{attr}'",
        ci_config::type_name(value)
    )
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

    /// `plan` with nothing interesting on the host and a counting `rand`.
    fn plan_of(cfg: &Value) -> Result<Vec<Step>, String> {
        plan_with(cfg, &State::default(), &json!([]), None)
    }

    fn plan_with(
        cfg: &Value,
        state: &State,
        args: &Value,
        default_user: Option<&Value>,
    ) -> Result<Vec<Step>, String> {
        let mut counter = 0;
        let mut rand = move || {
            counter += 1;
            format!("<random {counter}>")
        };
        plan(
            cfg.as_object().unwrap(),
            state,
            args,
            default_user,
            &mut rand,
            &mut Logger::silent(),
        )
    }

    fn chpasswd_of(steps: &[Step]) -> Vec<(bool, Vec<(String, String)>)> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Chpasswd { entries, hashed } => Some((
                    *hashed,
                    entries
                        .iter()
                        .map(|(name, secret)| (py_str(name), py_str(secret)))
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    fn expired_of(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::ExpirePasswd(user) => Some(py_str(user)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_null_password_becomes_the_literal_string_none() {
        // `get_cfg_option_str` runs `str()` over whatever is there, and
        // `str(None)` is a four-character password that is not empty and so
        // not skipped.
        let cfg = json!({"password": null, "users": ["default"]});
        let steps = plan_with(
            &cfg,
            &State::default(),
            &json!([]),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap();
        assert_eq!(
            chpasswd_of(&steps),
            vec![(false, vec![("ubuntu".to_owned(), "None".to_owned())])]
        );
    }

    #[test]
    fn an_empty_password_reaches_nobody() {
        let cfg = json!({"password": "", "users": ["default"]});
        assert_eq!(
            plan_with(
                &cfg,
                &State::default(),
                &json!([]),
                Some(&json!({"name": "ubuntu"}))
            )
            .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn a_command_line_password_wins_over_the_deprecated_list() {
        let cfg = json!({
            "chpasswd": {"list": ["alice:fromcfg"]},
            "users": ["default"],
        });
        let steps = plan_with(
            &cfg,
            &State::default(),
            &json!(["fromcli"]),
            Some(&json!({"name": "ubuntu"})),
        )
        .unwrap();
        assert_eq!(
            chpasswd_of(&steps),
            vec![(false, vec![("ubuntu".to_owned(), "fromcli".to_owned())])]
        );
    }

    #[test]
    fn deleting_that_list_off_a_string_chpasswd_is_a_type_error() {
        let cfg = json!({"chpasswd": "has a list in it"});
        assert_eq!(
            plan_with(&cfg, &State::default(), &json!(["x"]), None),
            Err("'str' object does not support item deletion".to_owned())
        );
    }

    #[test]
    fn a_chpasswd_that_is_not_a_container_at_all_is_a_type_error() {
        let cfg = json!({"chpasswd": 5});
        assert_eq!(
            plan_of(&cfg),
            Err("argument of type 'int' is not a container or iterable".to_owned())
        );
    }

    #[test]
    fn a_list_chpasswd_holding_users_is_indexed_and_fails_there() {
        let cfg = json!({"chpasswd": ["users"]});
        assert_eq!(
            plan_of(&cfg),
            Err("list indices must be integers or slices, not str".to_owned())
        );
    }

    #[test]
    fn an_entry_without_a_name_raises_the_bare_key_error() {
        // `str(KeyError("name"))` is `"'name'"` — the repr, with no prefix.
        let cfg = json!({"chpasswd": {"users": [{"password": "p"}]}});
        assert_eq!(plan_of(&cfg), Err("'name'".to_owned()));
    }

    #[test]
    fn users_are_ordered_text_then_hash_then_random() {
        let cfg = json!({"chpasswd": {"users": [
            {"name": "r", "type": "RANDOM"},
            {"name": "h", "password": "$6$a$b"},
            {"name": "t", "password": "p", "type": "text"},
        ]}});
        let steps = plan_of(&cfg).unwrap();
        assert_eq!(
            chpasswd_of(&steps),
            vec![
                (
                    false,
                    vec![
                        ("t".to_owned(), "p".to_owned()),
                        ("r".to_owned(), "<random 1>".to_owned()),
                    ]
                ),
                (true, vec![("h".to_owned(), "$6$a$b".to_owned())]),
            ]
        );
    }

    #[test]
    fn an_unknown_type_drops_the_entry_without_complaint() {
        let cfg = json!({"chpasswd": {"users": [
            {"name": "bob", "password": "p", "type": "weird"},
        ]}});
        assert_eq!(plan_of(&cfg).unwrap(), Vec::new());
    }

    #[test]
    fn a_hash_in_the_deprecated_list_is_told_apart_from_a_password() {
        for (secret, hashed) in [
            ("$6$salt$hash", true),
            ("$1$salt$hash", true),
            ("$2a$salt$hash", true),
            ("$2y$salt$hash", true),
            ("$5$salt$hash", true),
            // Not one of the five identifiers.
            ("$3$salt$hash", false),
            // Only one `$` after the identifier.
            ("$6$onlyone", false),
            // A colon anywhere disqualifies it, whatever the shape.
            ("$6$salt$hash:more", false),
            ("plain", false),
        ] {
            let cfg = json!({"chpasswd": {"list": [format!("bob:{secret}")]}});
            let steps = plan_of(&cfg).unwrap();
            assert_eq!(
                chpasswd_of(&steps),
                vec![(hashed, vec![("bob".to_owned(), secret.to_owned())])],
                "{secret}"
            );
        }
    }

    #[test]
    fn a_line_in_that_list_without_a_colon_cannot_be_unpacked() {
        let cfg = json!({"chpasswd": {"list": ["nocolon"]}});
        assert_eq!(
            plan_of(&cfg),
            Err("not enough values to unpack (expected 2, got 1)".to_owned())
        );
    }

    #[test]
    fn both_spellings_of_random_in_that_list_draw_a_password() {
        let cfg = json!({"chpasswd": {"list": ["a:R", "b:RANDOM"]}});
        let steps = plan_of(&cfg).unwrap();
        assert!(steps.contains(&Step::AnnounceRandom(
            "Set the following 'random' passwords\n\na:<random 1>\nb:<random 2>\n"
                .to_owned()
        )));
    }

    #[test]
    fn hashed_users_are_expired_too() {
        let cfg = json!({"chpasswd": {"users": [
            {"name": "t", "password": "p", "type": "text"},
            {"name": "h", "password": "$6$a$b"},
        ]}});
        assert_eq!(expired_of(&plan_of(&cfg).unwrap()), vec!["t", "h"]);
    }

    #[test]
    fn expire_false_leaves_every_password_usable() {
        let cfg = json!({"chpasswd": {
            "users": [{"name": "t", "password": "p", "type": "text"}],
            "expire": false,
        }});
        assert!(expired_of(&plan_of(&cfg).unwrap()).is_empty());
    }

    #[test]
    fn an_explicitly_null_ssh_pwauth_is_the_same_as_an_absent_one() {
        assert_eq!(plan_of(&json!({"ssh_pwauth": null})).unwrap(), Vec::new());
        assert_eq!(plan_of(&json!({})).unwrap(), Vec::new());
    }

    #[test]
    fn an_unrecognised_ssh_pwauth_string_only_warns() {
        for value in ["unchanged", "UNCHANGED", "maybe"] {
            assert_eq!(plan_of(&json!({"ssh_pwauth": value})).unwrap(), Vec::new());
        }
    }

    #[test]
    fn a_number_for_ssh_pwauth_reaches_upstreams_missing_lower() {
        // Upstream bug B67: neither true nor false, and not a string, so
        // `pw_auth.lower()` runs on an int and raises.
        assert_eq!(
            plan_of(&json!({"ssh_pwauth": 2})).unwrap(),
            vec![Step::Abort(
                "'int' object has no attribute 'lower'".to_owned()
            )]
        );
        assert_eq!(
            plan_of(&json!({"ssh_pwauth": []})).unwrap(),
            vec![Step::Abort(
                "'list' object has no attribute 'lower'".to_owned()
            )]
        );
    }

    #[test]
    fn the_abort_still_happens_after_the_passwords_are_set() {
        let cfg = json!({
            "chpasswd": {"users": [{"name": "t", "password": "p", "type": "text"}]},
            "ssh_pwauth": 2,
        });
        let steps = plan_of(&cfg).unwrap();
        assert!(matches!(steps.first(), Some(Step::Chpasswd { .. })));
        assert!(matches!(steps.last(), Some(Step::Abort(_))));
    }

    #[test]
    fn sshd_is_restarted_only_when_the_file_moved_and_the_unit_is_up() {
        let restarts = |updated: bool, systemd: bool, active: &str| {
            let state = State {
                ssh_svcname: "sshd".to_owned(),
                systemd,
                sshd_config_changes: updated,
                ssh_active_state: active.to_owned(),
            };
            plan_with(&json!({"ssh_pwauth": true}), &state, &json!([]), None)
                .unwrap()
                .into_iter()
                .filter(|step| matches!(step, Step::RestartSsh { .. }))
                .collect::<Vec<_>>()
        };
        assert!(restarts(false, true, "active").is_empty());
        assert!(restarts(true, true, "inactive").is_empty());
        assert_eq!(
            restarts(true, true, "Activating"),
            vec![Step::RestartSsh {
                service: "sshd".to_owned(),
                ignore_dependencies: true,
            }]
        );
        // Without systemd there is no deadlock to dodge, and no state to read.
        assert_eq!(
            restarts(true, false, ""),
            vec![Step::RestartSsh {
                service: "sshd".to_owned(),
                ignore_dependencies: false,
            }]
        );
    }

    #[test]
    fn a_generated_password_has_one_character_from_each_class() {
        let secret = rand_password().unwrap();
        assert_eq!(secret.chars().count(), PASSWORD_LEN);
        for set in ALPHABET {
            assert!(
                secret.bytes().any(|byte| set.contains(&byte)),
                "{secret} misses a class"
            );
        }
        assert_ne!(secret, rand_password().unwrap());
    }

    #[test]
    fn a_non_string_user_fails_the_way_subp_fails() {
        let err = expire_passwd(&json!(5), false).unwrap_err();
        assert!(
            err.starts_with("Unexpected error while running command.\nCommand: ['passwd', '--expire', 5]"),
            "{err}"
        );
    }

    #[test]
    fn a_non_string_in_a_pair_dies_in_the_join() {
        assert_eq!(
            chpasswd(&[(json!("bob"), json!(5))], false, false),
            Err("sequence item 1: expected str instance, int found".to_owned())
        );
        assert_eq!(
            chpasswd(&[(json!(5), json!("p"))], false, false),
            Err("sequence item 0: expected str instance, int found".to_owned())
        );
    }
}
