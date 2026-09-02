//! Atomic, permission-explicit file creation.
//!
//! Files are created with their final mode *before* any content is written, so a
//! secret never exists on disk with a wider mode than intended, and are published
//! with `rename(2)` so readers never observe a partial file.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// How to publish a file.
#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// Permission bits applied at creation time.
    pub mode: u32,
    /// `fsync` the file and its parent directory before returning.
    ///
    /// Required for anything that must survive a crash mid-boot (state, semaphores);
    /// can be disabled for throwaway files under `/run`.
    pub durable: bool,
}

impl WriteOptions {
    /// World-readable config/state, e.g. `/run/cloud-init/instance-data.json`.
    pub const PUBLIC: Self = Self {
        mode: 0o644,
        durable: true,
    };
    /// Root-only, e.g. `instance-data-sensitive.json` or rendered netplan.
    pub const SECRET: Self = Self {
        mode: 0o600,
        durable: true,
    };

    pub const fn mode(mode: u32) -> Self {
        Self {
            mode,
            durable: true,
        }
    }

    /// Skip the `fsync` calls; only for files that may be lost on power failure.
    #[must_use]
    pub const fn volatile(mut self) -> Self {
        self.durable = false;
        self
    }
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self::PUBLIC
    }
}

/// Atomically create or replace `path` with `contents`.
pub fn write_file(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    opts: WriteOptions,
) -> io::Result<()> {
    let path = path.as_ref();
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let tmp = temp_sibling(path)?;

    let result = write_temp(&tmp, contents.as_ref(), opts)
        .and_then(|()| std::fs::rename(&tmp, path))
        .and_then(|()| if opts.durable { sync_dir(dir) } else { Ok(()) });

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_temp(tmp: &Path, contents: &[u8], opts: WriteOptions) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(opts.mode)
        .open(tmp)?;
    file.write_all(contents)?;
    file.flush()?;
    if opts.durable {
        file.sync_all()?;
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    // An empty parent means the caller passed a bare filename; that is the cwd.
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    File::open(dir)?.sync_all()
}

/// A temp name in the *same* directory, so `rename` stays on one filesystem.
fn temp_sibling(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{seq}.tmp", process::id()));
    Ok(path.with_file_name(tmp_name))
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_with_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.json");
        write_file(&target, b"{}", WriteOptions::SECRET).unwrap();

        let meta = std::fs::metadata(&target).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"{}");
    }

    #[test]
    fn replaces_existing_file_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("status.json");
        write_file(&target, b"old", WriteOptions::PUBLIC).unwrap();
        write_file(&target, b"new", WriteOptions::PUBLIC).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from("status.json")]);
    }
}
