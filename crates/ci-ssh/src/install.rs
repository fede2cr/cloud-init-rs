//! The half of `cloudinit/ssh_util.py` that touches the filesystem: deciding
//! which file sshd will actually read for a user, making it safe to write, and
//! writing it.
//!
//! Every entry point takes a `root: &Path`. Logical paths — the ones that come
//! out of `sshd_config` and out of `/etc/passwd` — stay unrooted so that the
//! string comparisons upstream makes (`home.startswith(parent)`, `%h`
//! expansion) behave identically; `root` is applied only at the moment of a
//! syscall. A test root is then a real sandbox rather than a flag someone has
//! to remember, which matters more here than anywhere else in the port: the
//! obvious mistake in this file rewrites the developer's own
//! `~/.ssh/authorized_keys`.
//!
//! The permission checks are cloud-init's transcription of OpenSSH's
//! `safe_path()`. sshd will redo them at login, so they are advisory in
//! principle — but not in effect. A key installed into a path sshd refuses is
//! a key that silently does not work, and the whole point of the checks is to
//! notice in time and pick a different path.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use ci_log::{Level, Logger};

use crate::config;
use crate::AuthKeyLine;

const SOURCE: &str = "ssh_util.py";

/// `ssh_util.DEF_SSHD_CFG`.
pub const DEF_SSHD_CFG: &str = "/etc/ssh/sshd_config";

/// The ways looking up or installing a user's keys can fail outright.
///
/// Upstream splits these between exceptions it catches (`IOError`/`OSError`,
/// which demote a candidate path to "unusable") and exceptions it does not
/// (`KeyError` from the passwd and group databases, which abort the module).
/// The split is preserved: [`Error::Io`] is caught inside `check_create_path`,
/// the rest propagate.
#[derive(Debug)]
pub enum Error {
    /// `pwd.getpwnam` raised `KeyError`.
    UnknownUser(String),
    /// `users_ssh_info`'s own `RuntimeError`: a passwd record with no home.
    NoHome(String),
    /// A uid or gid on disk with no name behind it — a deleted account, or an
    /// id-mapped mount. Upstream raises `KeyError` from `pwd.getpwuid` /
    /// `grp.getgrgid` here and does not catch it either.
    UnknownId(String),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownUser(name) => write!(f, "Unknown user {name:?}"),
            Self::NoHome(name) => write!(f, "Unable to get SSH info for user {name:?}"),
            Self::UnknownId(what) => write!(f, "Unknown {what}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Resolve a logical absolute path against the target root.
fn at(root: &Path, logical: &str) -> PathBuf {
    root.join(logical.trim_start_matches('/'))
}

/// `os.path.join` for the one shape this module builds.
fn join(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// `os.path.dirname`.
fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(cut) => path.get(..cut).unwrap_or(""),
        None => "",
    }
}

/// `users_ssh_info`: the user's `.ssh` directory and their passwd record.
pub fn users_ssh_info(
    root: &Path,
    username: &str,
) -> Result<(String, ci_sys::ids::Passwd), Error> {
    let entry = ci_sys::ids::passwd_entry(root, username)
        .ok_or_else(|| Error::UnknownUser(username.to_owned()))?;
    if entry.dir.is_empty() {
        return Err(Error::NoHome(username.to_owned()));
    }
    Ok((join(&entry.dir, ".ssh"), entry))
}

/// `check_permissions`, cloud-init's port of OpenSSH's `misc.c:safe_path()`.
///
/// Three questions, in upstream's order: under `StrictModes`, is the path owned
/// by the user or by root; can the user reach it at all; and under
/// `StrictModes` again, is it group- or world-writable.
///
/// The middle question has an easily-mistranscribed shape. `minimal` starts as
/// the bits *anyone* would need — `0o711` to descend a directory, `0o644` to
/// read a file — and is then masked down to the single nibble that applies to
/// this user: owner, group, or other. What survives is one bit, and the path is
/// reachable if the mode has it.
fn check_permissions(
    root: &Path,
    username: &str,
    current_path: &str,
    full_path: &str,
    is_file: bool,
    strictmodes: bool,
    log: &mut Logger,
) -> Result<bool, Error> {
    use std::os::unix::fs::MetadataExt as _;

    let meta = std::fs::metadata(at(root, current_path))?;
    let owner = ci_sys::ids::user_name_for_uid(root, meta.uid())
        .ok_or_else(|| Error::UnknownId(format!("uid {}", meta.uid())))?;

    if strictmodes && owner != username && owner != "root" {
        log.log(Level::Debug, SOURCE, &format!(
            "Path {current_path} in {full_path} must be own by user {username} or by root, but instead is own by {owner}. Ignoring key."
        ));
        return Ok(false);
    }

    let mut minimal = if is_file { 0o644 } else { 0o711 };
    let permission = meta.mode() & 0o7777;

    if owner == username {
        minimal &= 0o700;
    } else {
        let group_owner = ci_sys::ids::group_name_for_gid(root, meta.gid())
            .ok_or_else(|| Error::UnknownId(format!("gid {}", meta.gid())))?;
        let user_groups = ci_sys::ids::user_groups(root, username)
            .ok_or_else(|| Error::UnknownUser(username.to_owned()))?;
        if user_groups.contains(&group_owner) {
            minimal &= 0o070;
        } else {
            minimal &= 0o007;
        }
    }

    if permission & minimal == 0 {
        log.log(Level::Debug, SOURCE, &format!(
            "Path {current_path} in {full_path} must be accessible by user {username}, check its permissions"
        ));
        return Ok(false);
    }

    if strictmodes && (permission & 0o022) != 0 {
        log.log(Level::Debug, SOURCE, &format!(
            "Path {current_path} in {full_path} must not give writepermission to group or world users. Ignoring key."
        ));
        return Ok(false);
    }

    Ok(true)
}

/// `check_create_path`: walk the path from `/` downwards, creating what is
/// missing and refusing anything unsafe.
///
/// A symlink anywhere along the way disqualifies the candidate rather than
/// being followed. That is the check that stops a user aiming their own
/// `AuthorizedKeysFile` at a file root would then create and chown to them.
///
/// Two components are skipped rather than checked: any ancestor of the home
/// directory, and the home directory itself. cloud-init is in no position to
/// make demands of `/home`, which it did not create.
fn check_create_path(
    root: &Path,
    username: &str,
    filename: &str,
    strictmodes: bool,
    log: &mut Logger,
) -> Result<bool, Error> {
    let user_pwent = users_ssh_info(root, username)?.1;
    let root_pwent = users_ssh_info(root, "root")?.1;

    let walk = Walk {
        root,
        username,
        filename,
        strictmodes,
        user_pwent: &user_pwent,
        root_pwent: &root_pwent,
    };
    match walk.run(log) {
        // Upstream's `except (IOError, OSError)`: a path that cannot be built
        // is a path this key does not go in, not a failed module.
        Err(Error::Io(err)) => {
            log.log(Level::Warning, SOURCE, &err.to_string());
            Ok(false)
        }
        other => other,
    }
}

/// The arguments of `check_create_path`'s `try` block, which has more inputs
/// than a free function should take positionally.
struct Walk<'a> {
    root: &'a Path,
    username: &'a str,
    filename: &'a str,
    strictmodes: bool,
    user_pwent: &'a ci_sys::ids::Passwd,
    root_pwent: &'a ci_sys::ids::Passwd,
}

impl Walk<'_> {
    fn run(&self, log: &mut Logger) -> Result<bool, Error> {
        // `filename.split("/")[1:-1]`: the directories, without the leading
        // empty field and without the basename.
        let mut components: Vec<&str> = self.filename.split('/').collect();
        components.pop();
        let home_folder = dirname(&self.user_pwent.dir);

        let mut parent_folder = String::new();
        for directory in components.into_iter().skip(1) {
            parent_folder.push('/');
            parent_folder.push_str(directory);

            if let Ok(meta) = std::fs::symlink_metadata(at(self.root, &parent_folder)) {
                if meta.is_symlink() {
                    log.log(
                        Level::Debug,
                        SOURCE,
                        &format!("Invalid directory. Symlink exists in path: {parent_folder}"),
                    );
                    return Ok(false);
                }
                if meta.is_file() {
                    log.log(
                        Level::Debug,
                        SOURCE,
                        &format!(
                            "Invalid directory. File exists in path: {parent_folder}"
                        ),
                    );
                    return Ok(false);
                }
            }

            if home_folder.starts_with(parent_folder.as_str())
                || parent_folder == self.user_pwent.dir
            {
                continue;
            }

            self.ensure_dir(&parent_folder)?;
            if !check_permissions(
                self.root,
                self.username,
                &parent_folder,
                self.filename,
                false,
                self.strictmodes,
                log,
            )? {
                return Ok(false);
            }
        }

        let real = at(self.root, self.filename);
        let meta = std::fs::symlink_metadata(&real);
        if meta.as_ref().is_ok_and(|m| m.is_symlink() || m.is_dir()) {
            log.log(
                Level::Debug,
                SOURCE,
                &format!("{} is not a file!", self.filename),
            );
            return Ok(false);
        }
        if meta.is_err() {
            if let Some(parent) = real.parent() {
                std::fs::create_dir_all(parent)?;
            }
            ci_sys::atomic::write_file(
                &real,
                "",
                ci_sys::atomic::WriteOptions::SECRET,
            )?;
            ci_sys::ids::chown_by_id(&real, self.user_pwent.uid, self.user_pwent.gid)?;
        }

        check_permissions(
            self.root,
            self.username,
            self.filename,
            self.filename,
            true,
            self.strictmodes,
            log,
        )
    }

    /// A missing directory is created shared and root-owned, unless it sits
    /// inside this user's home, where nobody else has any business looking.
    fn ensure_dir(&self, parent_folder: &str) -> Result<(), Error> {
        use std::os::unix::fs::DirBuilderExt as _;

        let real = at(self.root, parent_folder);
        if real.exists() {
            return Ok(());
        }
        let (mode, uid, gid) = if parent_folder.starts_with(&self.user_pwent.dir) {
            (0o700, self.user_pwent.uid, self.user_pwent.gid)
        } else {
            (0o755, self.root_pwent.uid, self.root_pwent.gid)
        };
        // `os.makedirs` only applies `mode` to the leaf; anything it has to
        // create on the way there gets the umask default.
        if let Some(parent) = real.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(mode).create(&real) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
        ci_sys::ids::chown_by_id(&real, uid, gid)?;
        Ok(())
    }
}

/// `parse_ssh_config_map`, against a real file.
///
/// A file that is not there is an empty map — that is `parse_ssh_config`'s
/// `os.path.isfile` guard, and it is how a distro with no `sshd_config` gets
/// the documented defaults. A file that is there but unreadable is an error;
/// see the note on upstream's handling of that case in `docs/COMPAT.md`.
fn read_sshd_config(
    root: &Path,
    sshd_cfg_file: &str,
    log: &mut Logger,
) -> Result<BTreeMap<String, String>, Error> {
    Ok(config::config_map(&parse_config_file(
        root,
        sshd_cfg_file,
        log,
    )?))
}

/// `parse_ssh_config`: an absent file is an empty config, not an error.
fn parse_config_file(
    root: &Path,
    sshd_cfg_file: &str,
    log: &mut Logger,
) -> Result<Vec<config::SshdConfigLine>, Error> {
    let path = at(root, sshd_cfg_file);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
    let lines = ci_core::pystr::split_lines(&text);
    Ok(config::parse_config_lines(&lines, log))
}

/// `parse_authorized_keys` for a single file: absent or unreadable is no keys,
/// with a note in the log. Upstream swallows the error here, and it is the
/// right call — refusing to add a key because the existing file is damaged
/// leaves the machine just as unreachable.
fn read_authorized_keys(
    root: &Path,
    filename: &str,
    log: &mut Logger,
) -> Vec<AuthKeyLine> {
    let path = at(root, filename);
    if !path.is_file() {
        return Vec::new();
    }
    match std::fs::read(&path) {
        Ok(bytes) => crate::parse_authorized_keys(&String::from_utf8_lossy(&bytes)),
        Err(err) => {
            log.log(
                Level::Warning,
                SOURCE,
                &format!("Error reading lines from {filename}: {err}"),
            );
            Vec::new()
        }
    }
}

/// `extract_authorized_keys`: the file sshd will read for `username`, and what
/// is already in it.
///
/// The candidate loop only considers paths that are per-user — either the
/// `sshd_config` token contained `%u`/`%h`, or the rendered path already lies
/// inside the home directory. A purely global `AuthorizedKeysFile` is left
/// alone: writing one tenant's key into a file every account shares would hand
/// them somebody else's login.
pub fn extract_authorized_keys(
    root: &Path,
    username: &str,
    sshd_cfg_file: &str,
    log: &mut Logger,
) -> Result<(String, Vec<AuthKeyLine>), Error> {
    let (ssh_dir, pw_ent) = users_ssh_info(root, username)?;
    let default_file = join(&ssh_dir, "authorized_keys");
    let mut chosen = default_file.clone();

    let cfg = read_sshd_config(root, sshd_cfg_file, log)?;
    let key_paths = cfg
        .get("authorizedkeysfile")
        .map_or("%h/.ssh/authorized_keys", String::as_str);
    let strictmodes = cfg.get("strictmodes").map_or("yes", String::as_str) == "yes";
    let auth_key_fns =
        crate::render_authorizedkeysfile_paths(key_paths, &pw_ent.dir, username);

    let home_prefix = format!("{}/", pw_ent.dir);
    for (key_path, auth_key_fn) in
        ci_core::pystr::split_whitespace_n(key_paths, usize::MAX)
            .into_iter()
            .zip(&auth_key_fns)
    {
        if !(key_path.contains("%u")
            || key_path.contains("%h")
            || auth_key_fn.starts_with(&home_prefix))
        {
            continue;
        }
        if check_create_path(root, username, auth_key_fn, strictmodes, log)? {
            chosen.clone_from(auth_key_fn);
            break;
        }
    }

    if chosen != default_file {
        log.log(
            Level::Debug,
            SOURCE,
            &format!("AuthorizedKeysFile has an user-specific authorized_keys, using {chosen}"),
        );
    }

    let existing = read_authorized_keys(root, &chosen, log);
    Ok((chosen, existing))
}

/// `setup_user_keys`: merge `keys` into whichever `authorized_keys` sshd reads
/// for `username`.
///
/// `options`, when non-empty, is forced onto every key regardless of what the
/// key line itself carried — that is how `ssh_redirect_user` installs the
/// default user's keys behind a banner that refuses the login.
///
/// The file is rewritten through its existing inode, not replaced, so it keeps
/// the owner and mode `check_create_path` gave it.
pub fn setup_user_keys(
    root: &Path,
    keys: &[String],
    username: &str,
    options: &str,
    log: &mut Logger,
) -> Result<(), Error> {
    let key_entries: Vec<AuthKeyLine> = keys
        .iter()
        .map(|key| crate::parse_auth_key_line(key, options))
        .collect();

    let (auth_key_fn, existing) =
        extract_authorized_keys(root, username, DEF_SSHD_CFG, log)?;
    let content = crate::update_authorized_keys(&existing, &key_entries);
    let real = at(root, &auth_key_fn);
    // `util.write_file`'s `ensure_dir_exists`. It matters: when no candidate
    // path survived `check_create_path` the default one is used without ever
    // having been created, and this is where `~/.ssh` comes into existence —
    // with the umask default rather than the 0700 the checked path gets.
    if let Some(parent) = real.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ci_sys::atomic::write_file_in_place(&real, content, 0o644)?;
    Ok(())
}

/// `_includes_dconf`: whether the main `sshd_config` defers to a drop-in
/// directory.
///
/// This decides *which file* everything below writes to. A wrong answer is not
/// a formatting difference: a setting written to a file sshd does not read is a
/// setting that silently did not apply, and one written above the `Include`
/// line is one a drop-in can still override.
///
/// The needle is built from the logical name, which is why logical paths stay
/// unrooted until the syscall.
fn includes_dconf(root: &Path, fname: &str) -> Result<bool, Error> {
    let main = at(root, fname);
    if !main.exists() && at(root, &format!("{fname}.d")).exists() {
        return Ok(true);
    }
    if !main.is_file() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&std::fs::read(&main)?).into_owned();
    let needle = format!("Include {fname}.d/*.conf");
    Ok(ci_core::pystr::split_lines(&text)
        .iter()
        .any(|line| line.starts_with(&needle)))
}

/// `_ensure_cloud_init_ssh_config_file`: the file cloud-init should write, and
/// the drop-in directory and empty root-only file to put it in if needed.
fn ensure_cloud_init_ssh_config_file(
    root: &Path,
    fname: &str,
) -> Result<String, Error> {
    if !includes_dconf(root, fname)? {
        return Ok(fname.to_owned());
    }
    let dname = format!("{fname}.d");
    let real_dir = at(root, &dname);
    if !real_dir.is_dir() {
        std::fs::create_dir_all(&real_dir)?;
        ci_sys::ids::set_mode(&real_dir, 0o755)?;
    }
    let target = join(&dname, "50-cloud-init.conf");
    if !at(root, &target).is_file() {
        ci_sys::atomic::append_file(at(root, &target), "", 0o600)?;
    }
    Ok(target)
}

/// `update_ssh_config_lines`: apply `updates` to a parsed config in place, and
/// report which keys actually moved.
///
/// A keyword already set to the wanted value is not a change. That is what
/// keeps the caller from rewriting the file — and restarting sshd — on every
/// boot, so the comparison matters more than it looks.
///
/// `updates` is a slice rather than a map because the order the missing
/// keywords get appended in is observable. It is folded to unique keys first,
/// the way the Python dict it stands in for would be — first position, last
/// value. Two entries differing only in *case* stay distinct, and reproduce
/// what upstream does with such a dict: the line is updated once, and the
/// shadowed spelling is appended anyway.
pub fn update_ssh_config_lines(
    lines: &mut Vec<config::SshdConfigLine>,
    updates: &[(&str, &str)],
    log: &mut Logger,
) -> Vec<String> {
    let mut wanted: Vec<(&str, &str)> = Vec::new();
    for (key, value) in updates {
        match wanted.iter_mut().find(|(seen, _)| seen == key) {
            Some(slot) => slot.1 = value,
            None => wanted.push((key, value)),
        }
    }
    let mut casemap: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for (key, value) in &wanted {
        casemap.insert(key.to_lowercase(), (*key, *value));
    }

    let mut found: Vec<&str> = Vec::new();
    let mut changed: Vec<String> = Vec::new();

    for (index, line) in lines.iter_mut().enumerate() {
        let Some(lower) = line.key().filter(|key| !key.is_empty()) else {
            continue;
        };
        let Some((key, value)) = casemap.get(&lower) else {
            continue;
        };
        if !found.contains(key) {
            found.push(key);
        }
        let number = index + 1;
        if line.value.as_deref() == Some(*value) {
            log.log(
                Level::Debug,
                SOURCE,
                &format!("line {number}: option {key} already set to {value}"),
            );
        } else {
            let old = line.value.clone().unwrap_or_default();
            changed.push((*key).to_owned());
            log.log(
                Level::Debug,
                SOURCE,
                &format!("line {number}: option {key} updated {old} -> {value}"),
            );
            line.value = Some((*value).to_owned());
        }
    }

    if found.len() != wanted.len() {
        for (key, value) in &wanted {
            if found.contains(key) {
                continue;
            }
            changed.push((*key).to_owned());
            lines.push(config::SshdConfigLine::new("", key, value));
            log.log(
                Level::Debug,
                SOURCE,
                &format!("line {}: option {key} added with {value}", lines.len()),
            );
        }
    }
    changed
}

/// Whether [`update_ssh_config`] would write anything, without writing it.
///
/// `cc_set_passwords` restarts `sshd` only when the file actually moved, and
/// it plans that restart before it acts, so it needs the answer up front. Not
/// side-effect free: like the write itself it settles which file is the target
/// first, which can create an empty `50-cloud-init.conf` drop-in.
pub fn would_update_ssh_config(
    root: &Path,
    updates: &[(&str, &str)],
    fname: &str,
    log: &mut Logger,
) -> Result<bool, Error> {
    let fname = ensure_cloud_init_ssh_config_file(root, fname)?;
    let mut lines = parse_config_file(root, &fname, log)?;
    Ok(!update_ssh_config_lines(&mut lines, updates, log).is_empty())
}

/// `update_ssh_config`: read, apply, and write back only if something moved.
pub fn update_ssh_config(
    root: &Path,
    updates: &[(&str, &str)],
    fname: &str,
    log: &mut Logger,
) -> Result<bool, Error> {
    let fname = ensure_cloud_init_ssh_config_file(root, fname)?;
    let mut lines = parse_config_file(root, &fname, log)?;
    if update_ssh_config_lines(&mut lines, updates, log).is_empty() {
        return Ok(false);
    }
    let mut body = lines
        .iter()
        .map(config::SshdConfigLine::render)
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    write_config(root, &fname, &body, false)?;
    Ok(true)
}

/// `append_ssh_config`. Appending rather than updating is deliberate upstream:
/// its one caller adds `HostCertificate` lines, and sshd takes the *last* one.
pub fn append_ssh_config(
    root: &Path,
    lines: &[(String, String)],
    fname: &str,
) -> Result<(), Error> {
    if lines.is_empty() {
        return Ok(());
    }
    let fname = ensure_cloud_init_ssh_config_file(root, fname)?;
    let mut body = lines
        .iter()
        .map(|(key, value)| format!("{key} {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    body.push('\n');
    write_config(root, &fname, &body, true)
}

/// `util.write_file(..., preserve_mode=True)` for the two shapes above: create
/// the parent, keep the mode of a file that already exists, and fall back to
/// 0o644 for one that does not.
fn write_config(
    root: &Path,
    fname: &str,
    body: &str,
    append: bool,
) -> Result<(), Error> {
    let path = at(root, fname);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if append {
        ci_sys::atomic::append_file(&path, body, 0o644)?;
    } else {
        ci_sys::atomic::write_file_in_place(&path, body, 0o644)?;
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

    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2E";

    /// The ids a file created by this process ends up with. The fixture hands
    /// them to `alice` so that the ownership checks run for real without the
    /// test needing to be root.
    fn own_ids(dir: &Path) -> (u32, u32) {
        use std::os::unix::fs::MetadataExt as _;
        let probe = dir.join(".probe");
        std::fs::write(&probe, "").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        std::fs::remove_file(&probe).unwrap();
        (meta.uid(), meta.gid())
    }

    /// `alice` is listed before `root` on purpose: when the tests *are* run as
    /// root the two share uid 0, and the reverse lookup has to settle on the
    /// name the assertions use.
    fn fixture() -> tempfile::TempDir {
        use std::os::unix::fs::DirBuilderExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (uid, gid) = own_ids(root);
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/passwd"),
            format!(
                "alice:x:{uid}:{gid}:Alice:/home/alice:/bin/sh\n\
                 root:x:0:0:root:/root:/bin/sh\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("etc/group"),
            format!("alice:x:{gid}:\nroot:x:0:\nsudo:x:4242:alice\n"),
        )
        .unwrap();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(root.join("home/alice"))
            .unwrap();
        dir
    }

    fn write_sshd_config(root: &Path, body: &str) {
        std::fs::create_dir_all(root.join("etc/ssh")).unwrap();
        std::fs::write(root.join("etc/ssh/sshd_config"), body).unwrap();
    }

    #[test]
    fn a_missing_user_is_reported_not_guessed() {
        let dir = fixture();
        let err = users_ssh_info(dir.path(), "nosuch").unwrap_err();
        assert!(matches!(err, Error::UnknownUser(_)), "{err:?}");
    }

    #[test]
    fn the_ssh_dir_and_the_key_file_are_created_closed() {
        let dir = fixture();
        let mut log = Logger::silent();
        setup_user_keys(
            dir.path(),
            &[format!("{RSA} alice@host")],
            "alice",
            "",
            &mut log,
        )
        .unwrap();

        let written = dir.path().join("home/alice/.ssh/authorized_keys");
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            format!("{RSA} alice@host\n")
        );
        assert_eq!(ci_sys::ids::mode_of(&written).unwrap(), 0o600);
        assert_eq!(
            ci_sys::ids::mode_of(&dir.path().join("home/alice/.ssh")).unwrap(),
            0o700
        );
    }

    #[test]
    fn a_second_run_keeps_the_mode_the_first_left() {
        let dir = fixture();
        let mut log = Logger::silent();
        setup_user_keys(dir.path(), &[format!("{RSA} first")], "alice", "", &mut log)
            .unwrap();

        let written = dir.path().join("home/alice/.ssh/authorized_keys");
        ci_sys::ids::set_mode(&written, 0o640).unwrap();

        setup_user_keys(
            dir.path(),
            &[format!("{RSA} second")],
            "alice",
            "",
            &mut log,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            format!("{RSA} second\n")
        );
        assert_eq!(ci_sys::ids::mode_of(&written).unwrap(), 0o640);
    }

    #[test]
    fn hand_written_lines_survive_a_rewrite() {
        let dir = fixture();
        let mut log = Logger::silent();
        std::fs::create_dir_all(dir.path().join("home/alice/.ssh")).unwrap();
        std::fs::write(
            dir.path().join("home/alice/.ssh/authorized_keys"),
            "# mine\nssh-rsa BBBB other\n",
        )
        .unwrap();

        setup_user_keys(dir.path(), &[format!("{RSA} new")], "alice", "", &mut log)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("home/alice/.ssh/authorized_keys"))
                .unwrap(),
            format!("# mine\nssh-rsa BBBB other\n{RSA} new\n")
        );
    }

    #[test]
    fn a_symlinked_key_file_is_refused_rather_than_followed() {
        let dir = fixture();
        let mut log = Logger::silent();
        std::fs::create_dir_all(dir.path().join("home/alice/.ssh")).unwrap();
        std::fs::write(dir.path().join("target"), "untouched").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("target"),
            dir.path().join("home/alice/.ssh/authorized_keys"),
        )
        .unwrap();

        let err = setup_user_keys(dir.path(), &[RSA.to_owned()], "alice", "", &mut log)
            .unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn a_global_authorizedkeysfile_is_left_alone() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "AuthorizedKeysFile /etc/ssh/global_keys\n");

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/authorized_keys");
        assert!(!dir.path().join("etc/ssh/global_keys").exists());
    }

    #[test]
    fn a_per_user_authorizedkeysfile_is_honoured() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "AuthorizedKeysFile %h/.ssh/ak\n");

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/ak");
    }

    #[test]
    fn strictmodes_rejects_a_group_writable_ssh_dir() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(
            dir.path(),
            "AuthorizedKeysFile %h/.ssh/ak\nStrictModes yes\n",
        );
        std::fs::create_dir_all(dir.path().join("home/alice/.ssh")).unwrap();
        ci_sys::ids::set_mode(&dir.path().join("home/alice/.ssh"), 0o770).unwrap();

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/authorized_keys");
        assert!(!dir.path().join("home/alice/.ssh/ak").exists());
    }

    #[test]
    fn strictmodes_off_accepts_what_strictmodes_on_refuses() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(
            dir.path(),
            "AuthorizedKeysFile %h/.ssh/ak\nStrictModes no\n",
        );
        std::fs::create_dir_all(dir.path().join("home/alice/.ssh")).unwrap();
        ci_sys::ids::set_mode(&dir.path().join("home/alice/.ssh"), 0o770).unwrap();

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/ak");
    }

    #[test]
    fn a_symlink_in_the_path_disqualifies_the_candidate() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "AuthorizedKeysFile %h/keys/ak\n");
        std::fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere"),
            dir.path().join("home/alice/keys"),
        )
        .unwrap();

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/authorized_keys");
        assert!(!dir.path().join("elsewhere/ak").exists());
    }

    #[test]
    fn the_first_usable_candidate_wins() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "AuthorizedKeysFile %h/keys/ak %h/.ssh/ak\n");
        std::fs::write(dir.path().join("home/alice/keys"), "in the way").unwrap();

        let (chosen, _) =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log)
                .unwrap();
        assert_eq!(chosen, "/home/alice/.ssh/ak");
    }

    #[test]
    fn an_unreadable_sshd_config_is_an_error_not_a_silent_default() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "AuthorizedKeysFile %h/.ssh/ak\n");
        let cfg = dir.path().join("etc/ssh/sshd_config");
        ci_sys::ids::set_mode(&cfg, 0o000).unwrap();

        let result =
            extract_authorized_keys(dir.path(), "alice", DEF_SSHD_CFG, &mut log);
        // Root can read a 0000 file, so the assertion only means anything
        // unprivileged.
        if own_ids(dir.path()).0 != 0 {
            assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
        }
    }

    #[test]
    fn a_config_without_an_include_line_is_written_directly() {
        let dir = fixture();
        write_sshd_config(dir.path(), "Port 22\n");
        assert_eq!(
            ensure_cloud_init_ssh_config_file(dir.path(), DEF_SSHD_CFG).unwrap(),
            DEF_SSHD_CFG,
        );
    }

    #[test]
    fn an_include_line_diverts_the_write_into_a_root_only_drop_in() {
        let dir = fixture();
        write_sshd_config(
            dir.path(),
            "Include /etc/ssh/sshd_config.d/*.conf\nPort 22\n",
        );
        let chosen =
            ensure_cloud_init_ssh_config_file(dir.path(), DEF_SSHD_CFG).unwrap();
        assert_eq!(chosen, "/etc/ssh/sshd_config.d/50-cloud-init.conf");

        let real = dir.path().join("etc/ssh/sshd_config.d/50-cloud-init.conf");
        assert!(real.is_file());
        assert_eq!(ci_sys::ids::mode_of(&real).unwrap(), 0o600);
        assert_eq!(
            ci_sys::ids::mode_of(&dir.path().join("etc/ssh/sshd_config.d")).unwrap(),
            0o755,
        );
    }

    #[test]
    fn a_drop_in_directory_with_no_main_config_is_enough() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("etc/ssh/sshd_config.d")).unwrap();
        assert_eq!(
            ensure_cloud_init_ssh_config_file(dir.path(), DEF_SSHD_CFG).unwrap(),
            "/etc/ssh/sshd_config.d/50-cloud-init.conf",
        );
    }

    #[test]
    fn an_unchanged_option_leaves_the_file_alone() {
        let dir = fixture();
        let mut log = Logger::silent();
        write_sshd_config(dir.path(), "Port 22\n");
        let path = dir.path().join("etc/ssh/sshd_config");
        ci_sys::ids::set_mode(&path, 0o640).unwrap();

        assert!(!update_ssh_config(
            dir.path(),
            &[("Port", "22")],
            DEF_SSHD_CFG,
            &mut log
        )
        .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Port 22\n");

        assert!(update_ssh_config(
            dir.path(),
            &[("Port", "2222")],
            DEF_SSHD_CFG,
            &mut log
        )
        .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Port 2222\n");
        // `preserve_mode=True`: rewriting must not widen a hardened config.
        assert_eq!(ci_sys::ids::mode_of(&path).unwrap(), 0o640);
    }

    #[test]
    fn appending_a_certificate_keeps_what_was_already_there() {
        let dir = fixture();
        write_sshd_config(dir.path(), "Port 22\n");
        append_ssh_config(
            dir.path(),
            &[("HostCertificate".to_owned(), "/etc/ssh/c.pub".to_owned())],
            DEF_SSHD_CFG,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("etc/ssh/sshd_config")).unwrap(),
            "Port 22\nHostCertificate /etc/ssh/c.pub\n",
        );
    }
}
