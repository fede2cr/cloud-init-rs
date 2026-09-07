//! Port of `util.is_container` and the helpers it consults.
//!
//! Callers use this to decide whether host-level facts are worth reading at
//! all: inside a container, DMI describes the host, not the instance.

use std::path::Path;

/// `util.is_container`.
///
/// Upstream memoises this with `lru_cache`; the checks are cheap enough that
/// the port just runs them, which also keeps the answer honest if the process
/// is re-execed into a different namespace.
#[must_use]
pub fn is_container() -> bool {
    is_container_at(Path::new("/proc"))
}

fn is_container_at(proc_dir: &Path) -> bool {
    if cmd_exits_zero(&["systemd-detect-virt", "--quiet", "--container"])
        || cmd_exits_zero(&["lxc-is-container"])
    {
        return true;
    }
    // `_is_container_freebsd` has nothing to check on Linux.
    let pid1_env = proc_env(&proc_dir.join("1/environ"));
    if pid1_env
        .iter()
        .any(|(name, _)| name == "container" || name == "LIBVIRT_LXC_UUID")
    {
        return true;
    }
    if proc_dir.join("vz").is_dir() && !proc_dir.join("bc").is_dir() {
        return true;
    }
    // Vserver.
    let status =
        std::fs::read_to_string(proc_dir.join("self/status")).unwrap_or_default();
    status.lines().any(|line| {
        line.strip_prefix("VxID:")
            .is_some_and(|value| value.trim() != "0")
    })
}

/// `util._cmd_exits_zero`.
fn cmd_exits_zero(argv: &[&str]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if ci_sys::subp::which(program).is_none() {
        return false;
    }
    ci_sys::Subp::new(argv).run().is_ok_and(|out| out.success())
}

/// `util.get_proc_env`, as name/value pairs.
#[must_use]
pub fn proc_env(path: &Path) -> Vec<(String, String)> {
    let Ok(raw) = std::fs::read(path) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|token| !token.is_empty())
        .filter_map(|token| {
            token
                .split_once('=')
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
        })
        .collect()
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

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_container_env_var_on_pid_one_is_enough() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("1/environ"), b"HOME=/\0container=lxc\0");
        assert!(is_container_at(dir.path()));
    }

    #[test]
    fn a_plain_proc_tree_is_not_a_container() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("1/environ"), b"HOME=/\0TERM=linux\0");
        write(&dir.path().join("self/status"), b"Name:\tcat\nVxID:\t0\n");
        // Only meaningful where the host is not itself a container.
        if !cmd_exits_zero(&["systemd-detect-virt", "--quiet", "--container"]) {
            assert!(!is_container_at(dir.path()));
        }
    }

    #[test]
    fn a_nonzero_vserver_id_is_a_container() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("self/status"), b"VxID:\t49\n");
        assert!(is_container_at(dir.path()));
    }

    #[test]
    fn an_environ_entry_without_an_equals_sign_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("1/environ"), b"bare\0A=1\0");
        assert_eq!(
            proc_env(&dir.path().join("1/environ")),
            [("A".to_owned(), "1".to_owned())]
        );
    }
}
