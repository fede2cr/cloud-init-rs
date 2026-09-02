//! Bounded, symlink-aware filesystem reads.
//!
//! cloud-init reads files it does not own (seed media, user-supplied config), so
//! every read is size-capped: an unbounded `read_to_string` on a hostile seed is a
//! trivial memory-exhaustion vector during early boot.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Default ceiling for a single config/seed file (8 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Read at most `max_bytes` from `path`, failing if the file is larger.
pub fn read_capped(path: impl AsRef<Path>, max_bytes: u64) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() > max_bytes {
        return Err(too_large(path, meta.len(), max_bytes));
    }

    // Re-check after reading: the file may have grown between stat and read.
    let mut buf = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    let read = file
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if read as u64 > max_bytes {
        return Err(too_large(path, read as u64, max_bytes));
    }
    Ok(buf)
}

/// Read at most `max_bytes` of UTF-8 text from `path`.
pub fn read_text_capped(path: impl AsRef<Path>, max_bytes: u64) -> io::Result<String> {
    let bytes = read_capped(path, max_bytes)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read a file, returning `Ok(None)` when it does not exist.
pub fn read_text_optional(
    path: impl AsRef<Path>,
    max_bytes: u64,
) -> io::Result<Option<String>> {
    match read_text_capped(path, max_bytes) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Create a private directory with a random name inside `parent`.
///
/// Created with `create_new` so an attacker cannot pre-place a symlink at the
/// path, and chmodded to 0700 before anything is written into it.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(parent: impl AsRef<Path>, prefix: &str) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;

        let parent = parent.as_ref();
        for _ in 0..64 {
            let token = crate::random_u64()?;
            let path = parent.join(format!("{prefix}{token:016x}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique temporary directory",
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Regular files directly inside `dir`, sorted by file name.
///
/// Symlinks are resolved for the file-type check but the returned path is the one
/// inside `dir`; callers that must not follow links should check separately.
pub fn list_files_sorted(dir: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().is_file() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn too_large(path: &Path, len: u64, max: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} is {len} bytes, exceeding the {max} byte limit",
            path.display()
        ),
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

    #[test]
    fn rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, vec![b'a'; 128]).unwrap();

        assert!(read_capped(&path, 64).is_err());
        assert_eq!(read_capped(&path, 128).unwrap().len(), 128);
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        assert!(read_text_optional(&path, DEFAULT_MAX_BYTES)
            .unwrap()
            .is_none());
    }
}
