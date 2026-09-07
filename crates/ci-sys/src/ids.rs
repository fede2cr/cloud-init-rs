//! User and group name lookup, by parsing `/etc/passwd` and `/etc/group`.
//!
//! `getpwnam(3)` would drag in either `unsafe` or a libc crate, and this crate
//! forbids the first and does without the second. The two files are a
//! documented colon-separated format and cloud-init only ever needs to resolve
//! names it shipped in its own contract, so parsing them directly is both
//! sufficient and easier to audit.
//!
//! NSS is deliberately not consulted. A name that only exists in LDAP or SSSD
//! will not resolve here, which is the safe direction: the caller reports "I
//! could not confirm this" rather than accepting an answer from a network
//! service that may be down or hostile during early boot.

use std::io;
use std::path::Path;

/// Numeric id of `name` in `<root>/etc/passwd`, or `None` if it is absent or
/// the file is unreadable.
pub fn uid_for_name(root: &Path, name: &str) -> Option<u32> {
    lookup(&root.join("etc/passwd"), name)
}

/// Numeric id of `name` in `<root>/etc/group`.
pub fn gid_for_name(root: &Path, name: &str) -> Option<u32> {
    lookup(&root.join("etc/group"), name)
}

/// The fields of an `/etc/passwd` record that cloud-init reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The home directory, as written — not canonicalised, and not joined to
    /// any root. Callers comparing it against other paths need the string.
    pub dir: String,
}

/// `pwd.getpwnam`, restricted to the fields that have a caller.
pub fn passwd_entry(root: &Path, name: &str) -> Option<Passwd> {
    let text = std::fs::read_to_string(root.join("etc/passwd")).ok()?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(&entry), Some(&uid), Some(&gid), Some(&dir)) =
            (fields.first(), fields.get(2), fields.get(3), fields.get(5))
        else {
            continue;
        };
        if entry != name {
            continue;
        }
        return Some(Passwd {
            name: entry.to_owned(),
            uid: uid.parse().ok()?,
            gid: gid.parse().ok()?,
            dir: dir.to_owned(),
        });
    }
    None
}

/// `util.get_owner`'s second half: `pwd.getpwuid(uid).pw_name`.
///
/// The first matching record wins, as `getpwuid` does. A uid with no record —
/// a file left behind by a deleted account, or an id-mapped mount — has no
/// name, and upstream raises `KeyError` there rather than degrading.
pub fn user_name_for_uid(root: &Path, uid: u32) -> Option<String> {
    name_for_id(&root.join("etc/passwd"), 2, uid)
}

/// `util.get_group`'s second half: `grp.getgrgid(gid).gr_name`.
pub fn group_name_for_gid(root: &Path, gid: u32) -> Option<String> {
    name_for_id(&root.join("etc/group"), 2, gid)
}

/// `util.get_user_groups`: every group listing `name` as a member, plus the
/// group named by the user's own primary gid.
///
/// `None` means the user has no passwd record or the primary gid has no group
/// record — both of which are a `KeyError` upstream, not an empty list.
pub fn user_groups(root: &Path, name: &str) -> Option<Vec<String>> {
    let mut groups = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("etc/group")) {
        for line in text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            let (Some(&group), Some(&members)) = (fields.first(), fields.get(3)) else {
                continue;
            };
            if members.split(',').any(|member| member == name) {
                groups.push(group.to_owned());
            }
        }
    }
    let entry = passwd_entry(root, name)?;
    groups.push(group_name_for_gid(root, entry.gid)?);
    Some(groups)
}

/// `util.chownbyid` against a numeric pair, without following a symlink.
pub fn chown_by_id(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

/// `os.chown(path, -1, gid)`: change the group and leave the owner alone.
pub fn set_group(path: &Path, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, None, Some(gid))
}

/// `util.chownbyname`, resolving against `<root>/etc/{passwd,group}`.
///
/// A `None` half leaves that half of the ownership alone, which is what
/// upstream's `-1` does. An unresolvable name is an error rather than a
/// silently skipped chown, matching the `OSError("Unknown user or group")`
/// upstream raises: a file that was asked to belong to someone else and
/// quietly stayed root-owned is worse than a loud failure.
///
/// The chown does not follow symlinks, so a symlink planted at `path` cannot
/// be used to hand ownership of an unrelated file to an unprivileged user.
pub fn chown_by_name(
    root: &Path,
    path: &Path,
    user: Option<&str>,
    group: Option<&str>,
) -> io::Result<()> {
    let uid = match user {
        None => None,
        Some(name) => Some(uid_for_name(root, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unknown user or group: '{name}'"),
            )
        })?),
    };
    let gid = match group {
        None => None,
        Some(name) => Some(gid_for_name(root, name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Unknown user or group: '{name}'"),
            )
        })?),
    };
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    std::os::unix::fs::lchown(path, uid, gid)
}

/// `util.get_permissions`: `stat.S_IMODE(os.stat(path).st_mode)`.
pub fn mode_of(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(path)?.mode() & 0o7777)
}

/// `util.chmod`.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Both files put the name first and the numeric id third. Anything else on
/// the line is another database's problem.
fn lookup(file: &Path, name: &str) -> Option<u32> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(name) {
            continue;
        }
        // Field 2 is the password placeholder, field 3 the id.
        let id = fields.nth(1)?;
        if let Ok(id) = id.parse() {
            return Some(id);
        }
    }
    None
}

/// [`lookup`] run backwards: the name of the first record whose field at
/// `id_field` is `id`.
fn name_for_id(file: &Path, id_field: usize, id: u32) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let (Some(&name), Some(&found)) = (fields.first(), fields.get(id_field)) else {
            continue;
        };
        if found.parse() == Ok(id) {
            return Some(name.to_owned());
        }
    }
    None
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

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/bash\n\
             syslog:x:104:110::/nonexistent:/usr/sbin/nologin\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("etc/group"), "root:x:0:\nadm:x:4:syslog\n")
            .unwrap();
        dir
    }

    #[test]
    fn names_resolve_to_their_numeric_ids() {
        let dir = fixture();
        assert_eq!(uid_for_name(dir.path(), "root"), Some(0));
        assert_eq!(uid_for_name(dir.path(), "syslog"), Some(104));
        assert_eq!(gid_for_name(dir.path(), "adm"), Some(4));
    }

    #[test]
    fn an_unknown_name_is_none_rather_than_a_guess() {
        let dir = fixture();
        assert_eq!(uid_for_name(dir.path(), "nobody"), None);
        assert_eq!(gid_for_name(dir.path(), "nobody"), None);
    }

    #[test]
    fn a_missing_database_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(uid_for_name(dir.path(), "root"), None);
    }
}
