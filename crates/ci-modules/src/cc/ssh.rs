//! Port of `cloudinit/config/cc_ssh.py`.
//!
//! The module that makes the machine reachable. It does three separable
//! things, in this order: it gives the instance its own host identity, it
//! offers that identity to the cloud so a client can verify it, and it puts
//! the tenant's keys where sshd will look for them.
//!
//! The first of those is the one with teeth. Every image ships with host keys
//! baked in at build time, and every instance launched from that image would
//! otherwise present the same ones — so `ssh_deletekeys` removes them and the
//! module generates a fresh set. Getting that wrong does not fail loudly: the
//! instance still boots, still accepts logins, and is still impersonable by
//! anyone else who launched the same image.
//!
//! Split into a [`plan`] and a [`run`] like the rest of this chain. Here the
//! reason is blunter than usual — carrying the plan out deletes files under
//! `/etc/ssh` and shells out to `ssh-keygen`, so the differential compares the
//! decisions and nothing else.

use ci_config::{option, Object, Value};
use ci_log::{Level, Logger};

use super::{py_str, Args};

const SOURCE: &str = "cc_ssh.py";

/// `GENERATE_KEY_NAMES`. Deliberately no `*-sk` types: those need a hardware
/// token touched by a human, which no first boot has.
const GENERATE_KEY_NAMES: [&str; 3] = ["rsa", "ecdsa", "ed25519"];

/// `FIPS_UNSUPPORTED_KEY_NAMES`.
const FIPS_UNSUPPORTED_KEY_NAMES: [&str; 1] = ["ed25519"];

/// `KEY_FILE_TPL`.
const KEY_FILE_TPL: &str = "/etc/ssh/ssh_host_%s_key";

/// The `ssh_keys:` suffixes, and the mode each lands with. Private keys must
/// not be group-readable; the certificate and the public key must be.
const SUFFIXES: [(&str, &str, u32); 3] = [
    ("_private", "", 0o600),
    ("_public", ".pub", 0o644),
    ("_certificate", "-cert.pub", 0o644),
];

/// `KEY_FILE_TPL % keytype`.
fn key_file(keytype: &str) -> String {
    KEY_FILE_TPL.replace("%s", keytype)
}

/// `CONFIG_KEY_TO_FILE[key]`: the target path and mode for an `ssh_keys:` entry.
fn config_key_to_file(key: &str) -> Option<(String, u32)> {
    for name in GENERATE_KEY_NAMES {
        for (suffix, extension, mode) in SUFFIXES {
            if key == format!("{name}{suffix}") {
                return Some((format!("{}{extension}", key_file(name)), mode));
            }
        }
    }
    None
}

/// `^(ecdsa-sk|ed25519-sk)_(private|public|certificate)$`, which upstream uses
/// only to pick the word in the warning.
fn is_unsupported_key(key: &str) -> bool {
    let Some((kind, part)) = key.split_once('_') else {
        return false;
    };
    matches!(kind, "ecdsa-sk" | "ed25519-sk")
        && matches!(part, "private" | "public" | "certificate")
}

/// The facts about the machine the plan depends on.
///
/// Upstream reads each of these inline. They are hoisted so the decisions
/// stay a pure function of config plus state, which is the only form the
/// harness can compare.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// `glob("/etc/ssh/ssh_host_*key*")`.
    ///
    /// Sorted, which upstream's is not — `glob` returns directory order. The
    /// only thing that order changes is the log, and every one of these files
    /// is deleted regardless.
    pub stale_key_files: Vec<String>,
    /// Which `KEY_FILE_TPL` paths already exist, so generation can skip them.
    ///
    /// Sampled before any step runs, so it still lists the keys
    /// `stale_key_files` is about to delete; `plan` subtracts those.
    pub existing_key_files: Vec<String>,
    /// `util.fips_enabled()`.
    pub fips: bool,
    /// `cloud.distro.osfamily == "redhat"`, which changes the mode and group a
    /// generated key ends up with.
    pub redhat: bool,
}

/// One action, in the order upstream would take it.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `util.del_file`. Failure is logged and stepped over.
    DeleteFile(String),
    /// `util.write_file` of an `ssh_keys:` entry.
    WriteKey {
        path: String,
        mode: u32,
        value: Value,
    },
    /// `ssh_util.append_ssh_config` of the `HostCertificate` lines.
    AppendSshConfig(Vec<(String, String)>),
    /// Derive the public key from a private one that arrived without it.
    KeygenPublic { private: String, public: String },
    /// `ssh-keygen -t <keytype> -N "" -f <keyfile>`.
    Keygen {
        keytype: Value,
        keyfile: String,
        quiet: bool,
        redhat_perms: bool,
    },
    /// Read `/etc/ssh/*.pub` and hand the result to the datasource.
    PublishHostKeys { blacklist: Vec<Value> },
    /// `ssh_util.setup_user_keys`.
    SetupUserKeys {
        user: String,
        keys: Vec<Value>,
        options: String,
    },
}

/// `handle`, minus everything that touches the system.
///
/// Returns `Err` only for the failures upstream lets escape. There are two
/// regions it deliberately does *not* return `Err` from, matching upstream's
/// two `try` blocks: publishing host keys, and applying credentials.
pub fn plan(
    cfg: &Object,
    state: &State,
    default_user: Option<&Value>,
    cloud_keys: &[String],
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();

    // Plain truthiness, not `util.get_cfg_option_bool`. `ssh_deletekeys: 0`
    // keeps the image's keys; `ssh_deletekeys: "false"` — the same intent,
    // quoted — deletes them, because a non-empty string is true.
    let mut deleted: Vec<&str> = Vec::new();
    if cfg.get("ssh_deletekeys").is_none_or(option::py_truthy) {
        for path in &state.stale_key_files {
            steps.push(Step::DeleteFile(path.clone()));
            deleted.push(path.as_str());
        }
    }

    match cfg.get("ssh_keys") {
        Some(keys) => supplied_keys(keys, &mut steps, log)?,
        None => generate_keys(cfg, state, &deleted, &mut steps, log)?,
    }

    let (blacklist, publish) = publish_settings(cfg)?;
    if publish {
        steps.push(Step::PublishHostKeys { blacklist });
    }

    // Upstream wraps everything below in one `try`, so a malformed `users:`
    // block costs more than the default user's keys: root's `authorized_keys`
    // is not written either, and the banner that disables root does not get
    // installed. That is reproduced, and it is worth knowing about.
    match credentials(cfg, default_user, cloud_keys, log) {
        Ok(mut applied) => steps.append(&mut applied),
        Err(err) => {
            log.log(
                Level::Warning,
                SOURCE,
                &format!("Applying SSH credentials failed!\n{err}"),
            );
        }
    }

    Ok(steps)
}

/// The `ssh_keys:` branch: install what the config supplied.
fn supplied_keys(
    keys: &Value,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> Result<(), String> {
    let Some(keys) = keys.as_object() else {
        return Err(format!(
            "'{}' object has no attribute 'items'",
            ci_config::type_name(keys)
        ));
    };

    let mut cert_config = Vec::new();
    for (key, value) in keys {
        let Some((path, mode)) = config_key_to_file(key) else {
            let reason = if is_unsupported_key(key) {
                "unsupported"
            } else {
                "unrecognized"
            };
            log.warning(
                SOURCE,
                &format!("Skipping {reason} ssh_keys entry: \"{key}\""),
            );
            continue;
        };
        if !value.is_string() {
            return Err(format!(
                "'{}' object has no attribute 'encode'",
                ci_config::type_name(value)
            ));
        }
        steps.push(Step::WriteKey {
            path: path.clone(),
            mode,
            value: value.clone(),
        });
        if key.contains("_certificate") {
            cert_config.push(("HostCertificate".to_owned(), path));
        }
    }
    if !cert_config.is_empty() {
        steps.push(Step::AppendSshConfig(cert_config));
    }

    // A private key with no public half beside it: derive one rather than
    // leave sshd without the file it advertises.
    for name in GENERATE_KEY_NAMES {
        let (private, public) = (format!("{name}_private"), format!("{name}_public"));
        if keys.contains_key(&public) || !keys.contains_key(&private) {
            continue;
        }
        steps.push(Step::KeygenPublic {
            private: key_file(name),
            public: format!("{}.pub", key_file(name)),
        });
    }
    Ok(())
}

/// The other branch: no keys were supplied, so make some.
fn generate_keys(
    cfg: &Object,
    state: &State,
    deleted: &[&str],
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> Result<(), String> {
    let genkeys = get_list(cfg, "ssh_genkeytypes").unwrap_or_else(|| {
        GENERATE_KEY_NAMES
            .iter()
            .map(|name| Value::String((*name).to_owned()))
            .collect()
    });
    // `set(genkeys)` is built whether or not anything is skipped, so a list or
    // a mapping in `ssh_genkeytypes:` aborts the module before a single key is
    // generated -- on a machine whose old keys have already been deleted.
    for key in &genkeys {
        if matches!(key, Value::Array(_) | Value::Object(_)) {
            return Err(unhashable(key));
        }
    }
    let wanted: Vec<&Value> = if state.fips {
        genkeys
            .iter()
            .filter(|key| {
                !key.as_str()
                    .is_some_and(|name| FIPS_UNSUPPORTED_KEY_NAMES.contains(&name))
            })
            .collect()
    } else {
        genkeys.iter().collect()
    };

    if wanted.len() != genkeys.len() {
        // Upstream joins a `set` difference, whose order is randomised per
        // process. Sorted here, which is the only order that can be compared.
        let mut skipped: Vec<String> = genkeys
            .iter()
            .filter(|key| !wanted.contains(key))
            .map(py_str)
            .collect();
        skipped.sort();
        skipped.dedup();
        log.log(
            Level::Debug,
            SOURCE,
            &format!(
                "skipping keys that are not supported in fips mode: {}",
                skipped.join(",")
            ),
        );
    }

    let quiet = option::get_bool(cfg, "ssh_quiet_keygen", false);
    for keytype in wanted {
        let keyfile = key_file(&py_str(keytype));
        // Upstream reaches its `os.path.exists(keyfile)` *after* the deletion
        // loop has already run. This plan is built before any step executes,
        // so a key the plan is about to delete must not count as present --
        // otherwise `ssh_deletekeys` removes every host key and generates
        // none, and sshd has nothing to start with.
        if state.existing_key_files.contains(&keyfile)
            && !deleted.contains(&keyfile.as_str())
        {
            continue;
        }
        // A non-string key type reaches `subp` inside the argv, which refuses
        // to run a command it cannot encode and raises the one exception this
        // block catches. The attempt is still made -- and still logged -- but
        // neither of the two decisions taken afterwards is reached, so the
        // output is never echoed and the permissions are never tightened.
        let rejected = !keytype.is_string();
        steps.push(Step::Keygen {
            keytype: keytype.clone(),
            keyfile,
            quiet: quiet || rejected,
            redhat_perms: state.redhat && !rejected,
        });
    }
    Ok(())
}

/// The `ssh_publish_hostkeys:` block, or its defaults.
///
/// The block is indexed as a mapping without being checked for being one, so
/// a list or a string here reaches Python's `in` operator with the wrong
/// meaning — membership, or a substring test — and then fails on the lookup.
/// All of it aborts the module.
fn publish_settings(cfg: &Object) -> Result<(Vec<Value>, bool), String> {
    let Some(block) = cfg.get("ssh_publish_hostkeys") else {
        return Ok((Vec::new(), true));
    };
    let blacklist = match sub_option(block, "blacklist")? {
        Some(value) => as_list(&value),
        None => Vec::new(),
    };
    let publish = match sub_option(block, "enabled")? {
        Some(value) => option::translate_bool(&value),
        None => true,
    };
    Ok((blacklist, publish))
}

/// `key in yobj` followed by `yobj[key]`, for a `yobj` that need not be a
/// mapping.
fn sub_option(block: &Value, key: &str) -> Result<Option<Value>, String> {
    match block {
        Value::Object(map) => Ok(map.get(key).cloned()),
        Value::Array(items) => {
            if items.iter().any(|item| item.as_str() == Some(key)) {
                Err("list indices must be integers or slices, not str".to_owned())
            } else {
                Ok(None)
            }
        }
        Value::String(text) => {
            if text.contains(key) {
                Err("string indices must be integers, not 'str'".to_owned())
            } else {
                Ok(None)
            }
        }
        other => Err(format!(
            "argument of type '{}' is not a container or iterable",
            ci_config::type_name(other)
        )),
    }
}

/// `util.get_cfg_option_list`'s conversion, minus the lookup.
fn as_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.clone(),
        Value::String(_) => vec![value.clone()],
        other => vec![Value::String(py_str(other))],
    }
}

/// `util.get_cfg_option_list(cfg, key, None)`.
fn get_list(cfg: &Object, key: &str) -> Option<Vec<Value>> {
    cfg.get(key).map(as_list)
}

/// `util.get_cfg_option_str`, which stringifies rather than refusing.
fn get_str(cfg: &Object, key: &str, default: &str) -> String {
    cfg.get(key).map_or_else(|| default.to_owned(), py_str)
}

/// The `apply_credentials` block, including the user normalisation upstream
/// put inside the same `try`.
fn credentials(
    cfg: &Object,
    default_user: Option<&Value>,
    cloud_keys: &[String],
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let ci_distro::ug::Normalized { mut users, .. } =
        ci_distro::ug::normalize_users_groups(cfg, default_user, log)?;
    let user = ci_distro::ug::extract_default(&mut users).map(|(name, _)| name);

    let disable_root = option::get_bool(cfg, "disable_root", true);
    let disable_root_opts =
        get_str(cfg, "disable_root_opts", &ci_ssh::disable_user_opts());

    let mut keys: Vec<Value> = if option::get_bool(cfg, "allow_public_ssh_keys", true) {
        cloud_keys
            .iter()
            .map(|key| Value::String(key.clone()))
            .collect()
    } else {
        log.log(
            Level::Debug,
            SOURCE,
            "Skipping import of publish SSH keys per config setting: \
             allow_public_ssh_keys=False",
        );
        Vec::new()
    };

    if let Some(extra) = cfg.get("ssh_authorized_keys") {
        // `list.extend`, so a string here contributes one key per character
        // and a mapping contributes its keys. Only a scalar is refused.
        match extra {
            Value::Array(items) => keys.extend(items.iter().cloned()),
            Value::String(text) => {
                keys.extend(text.chars().map(|ch| Value::String(ch.to_string())));
            }
            Value::Object(map) => {
                keys.extend(map.keys().map(|key| Value::String(key.clone())));
            }
            other => {
                return Err(format!(
                    "'{}' object is not iterable",
                    ci_config::type_name(other)
                ))
            }
        }
    }

    apply_credentials(&keys, user.as_deref(), disable_root, &disable_root_opts)
}

/// What `set()` says about something it cannot hash.
fn unhashable(value: &Value) -> String {
    let kind = ci_config::type_name(value);
    format!("cannot use '{kind}' as a set element (unhashable type: '{kind}')")
}

/// Python's `==` over the hashable scalars, which is not `serde_json`'s.
///
/// `True == 1` and `1 == 1.0` in Python, and they hash the same, so a `set`
/// keeps only the first of each pair. `serde_json` treats all three as
/// distinct values, which would leave duplicates in an authorized keys file
/// where upstream leaves one.
fn py_eq(left: &Value, right: &Value) -> bool {
    fn number(value: &Value) -> Option<f64> {
        match value {
            Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
            Value::Number(number) => number.as_f64(),
            _ => None,
        }
    }
    match (number(left), number(right)) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left == right,
        _ => false,
    }
}

/// `apply_credentials`.
///
/// Root is set up whether or not `disable_root` is on: with `disable_root` the
/// keys go in behind a banner that refuses the login and names the real
/// account, and without it they go in as ordinary keys. There is no path here
/// that leaves root's file untouched, which is why a config that disables root
/// still has to be able to write it.
fn apply_credentials(
    keys: &[Value],
    user: Option<&str>,
    disable_root: bool,
    disable_root_opts: &str,
) -> Result<Vec<Step>, String> {
    // `set(keys)`, so an unhashable entry aborts. Sorted and deduplicated for
    // the same reason `create_user`'s key list is: `set` iteration order is
    // randomised per process, and this is the only order comparable to it.
    let mut unique: Vec<Value> = Vec::new();
    for key in keys {
        if matches!(key, Value::Array(_) | Value::Object(_)) {
            return Err(unhashable(key));
        }
        if !unique.iter().any(|kept| py_eq(kept, key)) {
            unique.push(key.clone());
        }
    }
    // `str()` alone would tie `1` against `"1"` and `True` against `"True"`,
    // which survive the set separately; `repr` breaks the tie the same way
    // every run.
    unique.sort_by_key(|key| (py_str(key), ci_config::repr(key)));

    let mut steps = Vec::new();
    if let Some(user) = user {
        steps.push(Step::SetupUserKeys {
            user: user.to_owned(),
            keys: unique.clone(),
            options: String::new(),
        });
    }

    let key_prefix = if disable_root {
        disable_root_opts
            .replace("$USER", user.unwrap_or("NONE"))
            .replace("$DISABLE_USER", "root")
    } else {
        String::new()
    };
    steps.push(Step::SetupUserKeys {
        user: "root".to_owned(),
        keys: unique,
        options: key_prefix,
    });
    Ok(steps)
}

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let state = State {
        // `ssh_host_*key*` -- the trailing `*` matters: it takes the `.pub`
        // and `-cert.pub` halves with the private key, and leaving them
        // behind strands public keys that no longer match anything.
        stale_key_files: glob_host_keys(args.root, "ssh_host_", |rest| {
            rest.contains("key")
        }),
        existing_key_files: GENERATE_KEY_NAMES
            .iter()
            .map(|name| key_file(name))
            .filter(|path| at(args.root, path).exists())
            .collect(),
        fips: fips_enabled(args.root),
        redhat: args.distro.osfamily == "redhat",
    };
    let default_user = args.system_info.get("default_user").cloned();
    let cloud_keys: Vec<String> = args
        .datasource
        .as_ref()
        .map_or_else(Vec::new, |ds| ds.public_keys.to_vec());

    let steps = plan(
        args.cfg,
        &state,
        default_user.as_ref(),
        &cloud_keys,
        args.logger,
    )?;
    run(&steps, args.root, args.logger)
}

/// Carry the plan out.
pub fn run(
    steps: &[Step],
    root: &std::path::Path,
    log: &mut Logger,
) -> Result<(), String> {
    // The same guard the rest of the chain uses: `ssh-keygen` writes where its
    // arguments say and ignores any notion of a root, so a rooted run must not
    // reach it. A test that arrives here by accident stops instead of
    // regenerating the developer's own host keys.
    let live = root == std::path::Path::new("/");

    for step in steps {
        match step {
            Step::DeleteFile(path) => {
                let real = at(root, path);
                log.log(
                    Level::Debug,
                    SOURCE,
                    &format!("Attempting to remove {path}"),
                );
                if let Err(err) = std::fs::remove_file(&real) {
                    if err.kind() != std::io::ErrorKind::NotFound {
                        log.warning(
                            SOURCE,
                            &format!("Failed deleting key file {path}: {err}"),
                        );
                    }
                }
            }
            Step::WriteKey { path, mode, value } => {
                let real = at(root, path);
                if let Some(parent) = real.parent() {
                    ci_sys::path::ensure_dir(parent, 0o755)
                        .map_err(|err| format!("{}: {err}", parent.display()))?;
                }
                ci_sys::atomic::write_file(
                    &real,
                    value.as_str().unwrap_or_default(),
                    ci_sys::atomic::WriteOptions::mode(*mode),
                )
                .map_err(|err| format!("{path}: {err}"))?;
            }
            Step::AppendSshConfig(lines) => {
                ci_ssh::append_ssh_config(root, lines, ci_ssh::DEF_SSHD_CFG)
                    .map_err(|err| err.to_string())?;
            }
            Step::KeygenPublic { private, public } => {
                let script =
                    format!("o=$(ssh-keygen -yf \"{private}\") && echo \"$o\" root@localhost > \"{public}\"");
                match subp(&["sh".to_owned(), "-xc".to_owned(), script], live) {
                    Ok(()) => log.log(
                        Level::Debug,
                        SOURCE,
                        &format!("Generated a key for {public} from {private}"),
                    ),
                    Err(err) => log.warning(
                        SOURCE,
                        &format!(
                            "Failed generating a key for {public} from {private}: {err}"
                        ),
                    ),
                }
            }
            Step::Keygen {
                keytype,
                keyfile,
                quiet,
                redhat_perms,
            } => keygen(keytype, keyfile, *quiet, *redhat_perms, root, live, log),
            Step::PublishHostKeys { blacklist } => {
                let hostkeys = public_host_keys(root, blacklist);
                log.log(
                    Level::Debug,
                    SOURCE,
                    &format!("Would publish {} host key(s)", hostkeys.len()),
                );
            }
            Step::SetupUserKeys {
                user,
                keys,
                options,
            } => {
                let keys: Vec<String> = keys.iter().map(py_str).collect();
                if let Err(err) =
                    ci_ssh::setup_user_keys(root, &keys, user, options, log)
                {
                    log.warning(
                        SOURCE,
                        &format!("Applying SSH credentials failed!\n{err}"),
                    );
                }
            }
        }
    }
    Ok(())
}

fn keygen(
    keytype: &Value,
    keyfile: &str,
    quiet: bool,
    redhat_perms: bool,
    root: &std::path::Path,
    live: bool,
    log: &mut Logger,
) {
    let Some(keytype) = keytype.as_str() else {
        log.warning(
            SOURCE,
            &format!(
                "Failed generating key type {} to file {keyfile}",
                py_str(keytype)
            ),
        );
        return;
    };
    let real = at(root, keyfile);
    if let Some(parent) = real.parent() {
        let _ = ci_sys::path::ensure_dir(parent, 0o755);
    }
    let argv = [
        "ssh-keygen".to_owned(),
        "-t".to_owned(),
        keytype.to_owned(),
        "-N".to_owned(),
        String::new(),
        "-f".to_owned(),
        keyfile.to_owned(),
    ];
    match subp(&argv, live) {
        Ok(()) => {
            if !quiet {
                log.log(
                    Level::Debug,
                    SOURCE,
                    &format!("Generated a {keytype} host key in {keyfile}"),
                );
            }
            if redhat_perms {
                set_redhat_keyfile_perms(&real, root, log);
            }
        }
        Err(err) => log.warning(
            SOURCE,
            &format!("Failed generating key type {keytype} to file {keyfile}: {err}"),
        ),
    }
}

/// `set_redhat_keyfile_perms`, minus the sshd version probe.
///
/// Upstream asks `sshd -V` to decide between 0o640 and 0o600. The port keeps
/// the tighter of the two unconditionally: the looser mode exists only so that
/// a legacy `ssh_keys` group can read the key, and it is applied only when
/// that group is actually present, which is the check kept here.
fn set_redhat_keyfile_perms(
    keyfile: &std::path::Path,
    root: &std::path::Path,
    log: &mut Logger,
) {
    let mode = match ci_sys::ids::gid_for_name(root, "ssh_keys") {
        Some(gid) => {
            if let Err(err) = ci_sys::ids::set_group(keyfile, gid) {
                log.warning(
                    SOURCE,
                    &format!("Failed setting group on host key: {err}"),
                );
            }
            0o640
        }
        None => 0o600,
    };
    let _ = ci_sys::ids::set_mode(keyfile, mode);
    let pubkey = std::path::PathBuf::from(format!("{}.pub", keyfile.display()));
    let _ = ci_sys::ids::set_mode(&pubkey, 0o644);
}

/// `get_public_host_keys`: the first two fields of every `/etc/ssh/*.pub`
/// whose type is not blacklisted.
fn public_host_keys(
    root: &std::path::Path,
    blacklist: &[Value],
) -> Vec<(String, String)> {
    let excluded: Vec<String> = blacklist
        .iter()
        .map(|kind| format!("{}.pub", key_file(&py_str(kind))))
        .collect();
    let mut out = Vec::new();
    // `ssh_host_*_key.pub`, so the `*` may be empty but cannot swallow the
    // separator: `ssh_host_key.pub` is one character too short to match.
    for path in glob_host_keys(root, "ssh_host_", |rest| rest.ends_with("_key.pub")) {
        if excluded.contains(&path) {
            continue;
        }
        let Ok(bytes) = std::fs::read(at(root, &path)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let fields: Vec<&str> = text.split_whitespace().collect();
        if let (Some(kind), Some(data)) = (fields.first(), fields.get(1)) {
            out.push(((*kind).to_owned(), (*data).to_owned()));
        }
    }
    out
}

/// The two `glob` patterns this module uses, both anchored in `/etc/ssh`.
///
/// `rest_matches` is applied to what follows `prefix`, which is where both
/// patterns put their leading `*`.
///
/// Sorted, unlike `glob.glob`. See [`State::stale_key_files`].
fn glob_host_keys(
    root: &std::path::Path,
    prefix: &str,
    rest_matches: impl Fn(&str) -> bool,
) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(at(root, "/etc/ssh")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty() && rest_matches(rest))
        })
        .map(|name| format!("/etc/ssh/{name}"))
        .collect();
    out.sort();
    out
}

/// `util.fips_enabled()`.
fn fips_enabled(root: &std::path::Path) -> bool {
    std::fs::read_to_string(at(root, "/proc/sys/crypto/fips_enabled"))
        .is_ok_and(|text| text.trim() == "1")
}

fn at(root: &std::path::Path, logical: &str) -> std::path::PathBuf {
    root.join(logical.trim_start_matches('/'))
}

fn subp(argv: &[String], live: bool) -> Result<(), String> {
    if !live {
        return Err(format!(
            "refusing to run {:?} against a rooted tree",
            argv.first().map_or("", String::as_str)
        ));
    }
    ci_sys::subp::Subp::new(argv)
        .check()
        .map(|_| ())
        .map_err(|err| err.to_string())
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

    fn plan_of(cfg: &Value) -> Result<Vec<Step>, String> {
        plan(
            cfg.as_object().unwrap(),
            &State::default(),
            None,
            &[],
            &mut Logger::silent(),
        )
    }

    #[test]
    fn the_images_baked_in_keys_are_deleted_by_default() {
        let state = State {
            stale_key_files: vec!["/etc/ssh/ssh_host_rsa_key".to_owned()],
            ..State::default()
        };
        let steps =
            plan(&Object::new(), &state, None, &[], &mut Logger::silent()).unwrap();
        assert!(
            steps.contains(&Step::DeleteFile("/etc/ssh/ssh_host_rsa_key".to_owned()))
        );
    }

    /// The plan is built before any of it runs, so the keys it is about to
    /// delete are still on disk when it decides what to generate. Reading
    /// `existing_key_files` alone made the default configuration delete every
    /// host key and generate none, leaving sshd unable to start.
    #[test]
    fn a_key_that_is_deleted_is_regenerated_in_the_same_plan() {
        let state = State {
            stale_key_files: GENERATE_KEY_NAMES.iter().map(|n| key_file(n)).collect(),
            existing_key_files: GENERATE_KEY_NAMES
                .iter()
                .map(|n| key_file(n))
                .collect(),
            ..State::default()
        };
        let steps =
            plan(&Object::new(), &state, None, &[], &mut Logger::silent()).unwrap();

        for name in GENERATE_KEY_NAMES {
            let keyfile = key_file(name);
            assert!(
                steps.contains(&Step::DeleteFile(keyfile.clone())),
                "{name} should be deleted"
            );
            assert!(
                steps.iter().any(
                    |s| matches!(s, Step::Keygen { keyfile: f, .. } if *f == keyfile)
                ),
                "{name} was deleted but never regenerated"
            );
        }
    }

    /// The mirror image: keys that are on disk and are *not* being deleted are
    /// left alone, which is what `ssh_deletekeys: 0` buys.
    #[test]
    fn a_key_that_survives_is_not_regenerated() {
        let mut cfg = Object::new();
        cfg.insert("ssh_deletekeys".to_owned(), json!(0));
        let state = State {
            stale_key_files: GENERATE_KEY_NAMES.iter().map(|n| key_file(n)).collect(),
            existing_key_files: GENERATE_KEY_NAMES
                .iter()
                .map(|n| key_file(n))
                .collect(),
            ..State::default()
        };
        let steps = plan(&cfg, &state, None, &[], &mut Logger::silent()).unwrap();
        assert!(!steps.iter().any(|s| matches!(s, Step::DeleteFile(_))));
        assert!(!steps.iter().any(|s| matches!(s, Step::Keygen { .. })));
    }

    #[test]
    fn a_quoted_false_still_deletes_them() {
        let state = State {
            stale_key_files: vec!["/etc/ssh/ssh_host_rsa_key".to_owned()],
            ..State::default()
        };
        let deletes = |value: Value| {
            let mut cfg = Object::new();
            cfg.insert("ssh_deletekeys".to_owned(), value);
            plan(&cfg, &state, None, &[], &mut Logger::silent())
                .unwrap()
                .iter()
                .any(|step| matches!(step, Step::DeleteFile(_)))
        };
        assert!(!deletes(json!(false)));
        assert!(!deletes(json!(0)));
        // The one that surprises: a non-empty string is true.
        assert!(deletes(json!("false")));
    }

    #[test]
    fn three_key_types_are_generated_when_none_were_supplied() {
        let steps = plan_of(&json!({})).unwrap();
        let types: Vec<String> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Keygen { keytype, .. } => Some(py_str(keytype)),
                _ => None,
            })
            .collect();
        assert_eq!(types, ["rsa", "ecdsa", "ed25519"]);
    }

    #[test]
    fn fips_mode_drops_the_key_type_it_cannot_make() {
        let state = State {
            fips: true,
            ..State::default()
        };
        let steps =
            plan(&Object::new(), &state, None, &[], &mut Logger::silent()).unwrap();
        let types: Vec<String> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Keygen { keytype, .. } => Some(py_str(keytype)),
                _ => None,
            })
            .collect();
        assert_eq!(types, ["rsa", "ecdsa"]);
    }

    #[test]
    fn a_supplied_certificate_is_announced_in_sshd_config() {
        let steps = plan_of(&json!({
            "ssh_keys": {"rsa_certificate": "CERT", "rsa_private": "PRIV"},
        }))
        .unwrap();
        assert!(steps.contains(&Step::AppendSshConfig(vec![(
            "HostCertificate".to_owned(),
            "/etc/ssh/ssh_host_rsa_key-cert.pub".to_owned(),
        )])));
        // The private key arrived without its public half, so one is derived.
        assert!(steps.contains(&Step::KeygenPublic {
            private: "/etc/ssh/ssh_host_rsa_key".to_owned(),
            public: "/etc/ssh/ssh_host_rsa_key.pub".to_owned(),
        }));
        // And nothing is generated from scratch.
        assert!(!steps.iter().any(|s| matches!(s, Step::Keygen { .. })));
    }

    #[test]
    fn a_private_key_lands_closed_and_its_public_half_does_not() {
        let steps =
            plan_of(&json!({"ssh_keys": {"rsa_private": "P", "rsa_public": "Q"}}))
                .unwrap();
        let modes: Vec<u32> = steps
            .iter()
            .filter_map(|step| match step {
                Step::WriteKey { mode, .. } => Some(*mode),
                _ => None,
            })
            .collect();
        assert_eq!(modes, [0o600, 0o644]);
    }

    #[test]
    fn an_unknown_ssh_keys_entry_is_skipped_not_fatal() {
        let steps = plan_of(&json!({
            "ssh_keys": {"nonsense": "x", "ed25519-sk_private": "y", "rsa_private": "P"},
        }))
        .unwrap();
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, Step::WriteKey { .. }))
                .count(),
            1,
        );
    }

    #[test]
    fn root_is_always_set_up_and_disabled_by_default() {
        let steps = plan_of(&json!({})).unwrap();
        let root = steps
            .iter()
            .find_map(|step| match step {
                Step::SetupUserKeys { user, options, .. } if user == "root" => {
                    Some(options.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(root.contains(r#"the user \"NONE\""#), "{root}");
    }

    #[test]
    fn disable_root_off_installs_the_keys_bare() {
        let steps = plan_of(&json!({"disable_root": false})).unwrap();
        assert!(steps.contains(&Step::SetupUserKeys {
            user: "root".to_owned(),
            keys: Vec::new(),
            options: String::new(),
        }));
    }

    #[test]
    fn the_banner_names_the_default_user() {
        // The default user has to be *asked* for: an empty config normalises to
        // no users at all, and then root's banner names "NONE".
        let cfg = json!({"users": ["default"]});
        let steps = plan(
            cfg.as_object().unwrap(),
            &State::default(),
            Some(&json!({"name": "ubuntu"})),
            &[],
            &mut Logger::silent(),
        )
        .unwrap();
        let root = steps
            .iter()
            .find_map(|step| match step {
                Step::SetupUserKeys { user, options, .. } if user == "root" => {
                    Some(options.clone())
                }
                _ => None,
            })
            .unwrap();
        assert!(root.contains(r#"the user \"ubuntu\""#), "{root}");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::SetupUserKeys { user, .. } if user == "ubuntu"
        )));
    }

    #[test]
    fn a_string_of_authorized_keys_becomes_one_key_per_character() {
        let steps = plan_of(&json!({"ssh_authorized_keys": "abc"})).unwrap();
        let keys = steps
            .iter()
            .find_map(|step| match step {
                Step::SetupUserKeys { keys, .. } => Some(keys.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(keys, vec![json!("a"), json!("b"), json!("c")]);
    }

    #[test]
    fn a_scalar_authorized_keys_costs_root_its_keys_too() {
        // `list.extend(5)` raises inside the one `try` that also wraps root,
        // so the whole credentials block is skipped -- including the banner
        // that would have disabled root.
        let steps = plan_of(&json!({"ssh_authorized_keys": 5})).unwrap();
        assert!(!steps
            .iter()
            .any(|step| matches!(step, Step::SetupUserKeys { .. })));
    }

    #[test]
    fn publishing_is_on_unless_the_block_turns_it_off() {
        assert!(plan_of(&json!({}))
            .unwrap()
            .iter()
            .any(|s| matches!(s, Step::PublishHostKeys { .. })));
        assert!(
            !plan_of(&json!({"ssh_publish_hostkeys": {"enabled": false}}))
                .unwrap()
                .iter()
                .any(|s| matches!(s, Step::PublishHostKeys { .. }))
        );
    }

    #[test]
    fn a_publish_block_that_is_not_a_mapping_aborts_the_module() {
        assert!(plan_of(&json!({"ssh_publish_hostkeys": 5})).is_err());
        assert!(plan_of(&json!({"ssh_publish_hostkeys": ["enabled"]})).is_err());
        // ... but only once it is actually indexed.
        assert!(plan_of(&json!({"ssh_publish_hostkeys": ["other"]})).is_ok());
    }
}
