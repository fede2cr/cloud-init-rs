//! Python exception text, for the messages that reach `result.json`.
//!
//! Upstream records the `str()` of whatever exception aborted a stage, so the
//! wording of an `OSError` is part of the observable output rather than an
//! internal detail. Rust spells the same failure differently — "Permission
//! denied (os error 13)" against Python's "[Errno 13] Permission denied" — so
//! anything destined for `result.json` has to be translated on the way.

use std::path::Path;

/// `OSError.__str__` for the two-argument form: `[Errno N] strerror: 'path'`.
///
/// An error with no `errno` behind it — one Rust raised itself rather than one
/// the kernel returned — has no Python equivalent, so it is rendered as its
/// own message and nothing more.
#[must_use]
pub fn oserror(err: &std::io::Error, path: &Path) -> String {
    let Some(errno) = err.raw_os_error() else {
        return err.to_string();
    };
    // Rust appends " (os error N)" to every message it takes from `strerror`,
    // and that suffix is the only difference from what Python prints.
    let text = err.to_string();
    let strerror = text
        .strip_suffix(&format!(" (os error {errno})"))
        .unwrap_or(&text);
    format!("[Errno {errno}] {strerror}: '{}'", path.display())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn eacces_reads_the_way_python_prints_it() {
        let err = std::io::Error::from_raw_os_error(13);
        assert_eq!(
            oserror(&err, Path::new("/etc/netplan/50-cloud-init.yaml")),
            "[Errno 13] Permission denied: '/etc/netplan/50-cloud-init.yaml'"
        );
    }

    #[test]
    fn enoent_keeps_its_own_wording() {
        let err = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            oserror(&err, Path::new("/nope")),
            "[Errno 2] No such file or directory: '/nope'"
        );
    }

    #[test]
    fn an_error_without_an_errno_is_left_alone() {
        let err = std::io::Error::other("synthetic");
        assert_eq!(oserror(&err, Path::new("/nope")), "synthetic");
    }
}
