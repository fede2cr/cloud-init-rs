//! Port of `cloudinit/helpers.py`'s frequency engine.
//!
//! Every module and handler runs under a frequency. The semaphore file that
//! records a run is what makes `once` and `once-per-instance` stick across
//! boots, so its name and location are part of the on-disk contract with the
//! Python implementation.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::paths::{Lookup, Paths};

/// `PER_ALWAYS`, `PER_INSTANCE` and `PER_ONCE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Frequency {
    Always,
    Instance,
    Once,
}

impl Frequency {
    /// The string upstream writes in config and semaphore filenames.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Instance => "once-per-instance",
            Self::Once => "once",
        }
    }

    /// Parse a `frequency` value from config.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "always" => Some(Self::Always),
            "once-per-instance" => Some(Self::Instance),
            "once" => Some(Self::Once),
            _ => None,
        }
    }
}

impl fmt::Display for Frequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `canon_sem_name`.
fn canon_sem_name(name: &str) -> String {
    name.replace('-', "_")
}

/// A held semaphore. Dropping it does not clear the file; that is what makes
/// the run stick.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for FileLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<FileLock using file {:?}>", self.path.display())
    }
}

/// `FileSemaphores` — run markers in one directory.
#[derive(Debug, Clone)]
pub struct FileSemaphores {
    sem_path: PathBuf,
}

impl FileSemaphores {
    pub fn new(sem_path: impl Into<PathBuf>) -> Self {
        Self {
            sem_path: sem_path.into(),
        }
    }

    /// `_get_path`: `PER_INSTANCE` is bare, every other frequency is suffixed.
    pub fn path(&self, name: &str, freq: Frequency) -> PathBuf {
        let name = canon_sem_name(name);
        if freq == Frequency::Instance {
            self.sem_path.join(name)
        } else {
            self.sem_path.join(format!("{name}.{freq}"))
        }
    }

    /// `has_run`. `PER_ALWAYS` has never run, by definition.
    pub fn has_run(&self, name: &str, freq: Frequency) -> bool {
        if freq == Frequency::Always {
            return false;
        }
        self.path(name, freq).exists()
    }

    /// `_acquire`: `None` when the marker is already there.
    ///
    /// Upstream notes this is not atomic. It is not made atomic here either:
    /// the file must be observable to the Python implementation, and taking a
    /// real lock would change when the marker appears.
    pub fn acquire(&self, name: &str, freq: Frequency) -> io::Result<Option<FileLock>> {
        if self.has_run(name, freq) {
            return Ok(None);
        }
        let path = self.path(name, freq);
        let contents =
            format!("{}: {}\n", std::process::id(), crate::time::now_epoch());
        ci_sys::write_file(&path, contents.as_bytes(), ci_sys::WriteOptions::PUBLIC)?;
        Ok(Some(FileLock { path }))
    }

    /// `clear`: a missing marker still counts as cleared.
    pub fn clear(&self, name: &str, freq: Frequency) -> bool {
        let path = self.path(name, freq);
        match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }
}

/// `Runners` — picks the semaphore directory a frequency belongs in.
#[derive(Debug, Clone)]
pub struct Runners {
    paths: Paths,
    instance_id: Option<String>,
}

impl Runners {
    pub fn new(paths: Paths, instance_id: Option<String>) -> Self {
        Self { paths, instance_id }
    }

    /// `_get_sem`. `PER_ALWAYS` has no semaphore, and `PER_INSTANCE` has none
    /// until an instance id is known.
    pub fn semaphores(&self, freq: Frequency) -> Option<FileSemaphores> {
        match freq {
            Frequency::Always => None,
            Frequency::Once => Some(FileSemaphores::new(self.paths.cpath(Lookup::Sem))),
            Frequency::Instance => self.instance_id.as_deref().map(|iid| {
                FileSemaphores::new(self.paths.instance_path_for(iid, Lookup::Sem))
            }),
        }
    }

    /// `run`: execute `body` unless it has already run at this frequency.
    ///
    /// Returns `Ok(None)` when the semaphore says it already ran.
    pub fn run<T>(
        &self,
        name: &str,
        freq: Frequency,
        clear_on_fail: bool,
        body: impl FnOnce() -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let Some(sem) = self.semaphores(freq) else {
            return body().map(Some);
        };
        if sem.has_run(name, freq) {
            return Ok(None);
        }
        sem.acquire(name, freq)
            .map_err(|e| format!("Failed writing semaphore file: {e}"))?;
        match body() {
            Ok(value) => Ok(Some(value)),
            Err(e) => {
                if clear_on_fail {
                    sem.clear(name, freq);
                }
                Err(e)
            }
        }
    }
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

    fn sems(dir: &Path) -> FileSemaphores {
        std::fs::create_dir_all(dir).unwrap();
        FileSemaphores::new(dir)
    }

    #[test]
    fn per_instance_markers_are_unsuffixed() {
        let s = FileSemaphores::new("/var/lib/cloud/instance/sem");
        assert_eq!(
            s.path("config_mounts", Frequency::Instance),
            Path::new("/var/lib/cloud/instance/sem/config_mounts")
        );
    }

    #[test]
    fn other_frequencies_carry_the_frequency_as_a_suffix() {
        let s = FileSemaphores::new("/var/lib/cloud/sem");
        assert_eq!(
            s.path("config_growpart", Frequency::Once),
            Path::new("/var/lib/cloud/sem/config_growpart.once")
        );
        assert_eq!(
            s.path("apply_network_config", Frequency::Always),
            Path::new("/var/lib/cloud/sem/apply_network_config.always")
        );
    }

    #[test]
    fn dashes_in_a_name_become_underscores() {
        let s = FileSemaphores::new("/var/lib/cloud/sem");
        assert_eq!(
            s.path("config-ssh-keys", Frequency::Instance),
            Path::new("/var/lib/cloud/sem/config_ssh_keys")
        );
    }

    #[test]
    fn always_has_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let s = sems(dir.path());
        s.acquire("thing", Frequency::Always).unwrap();
        assert!(!s.has_run("thing", Frequency::Always));
    }

    #[test]
    fn acquiring_twice_yields_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = sems(dir.path());
        assert!(s.acquire("thing", Frequency::Once).unwrap().is_some());
        assert!(s.has_run("thing", Frequency::Once));
        assert!(s.acquire("thing", Frequency::Once).unwrap().is_none());
    }

    #[test]
    fn the_marker_records_the_pid_and_a_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let s = sems(dir.path());
        let lock = s.acquire("thing", Frequency::Once).unwrap().unwrap();
        let body = std::fs::read_to_string(lock.path()).unwrap();
        let (pid, rest) = body.trim_end().split_once(": ").unwrap();
        assert_eq!(pid.parse::<u32>().unwrap(), std::process::id());
        assert!(rest.parse::<f64>().unwrap() > 0.0);
    }

    #[test]
    fn clearing_a_missing_marker_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let s = sems(dir.path());
        assert!(s.clear("never", Frequency::Once));
    }

    #[test]
    fn a_run_is_skipped_once_its_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().to_path_buf(),
            ..Paths::default()
        };
        std::fs::create_dir_all(paths.cpath(Lookup::Sem)).unwrap();
        let runners = Runners::new(paths, None);

        assert_eq!(
            runners
                .run("thing", Frequency::Once, false, || Ok(1))
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            runners
                .run("thing", Frequency::Once, false, || Ok(2))
                .unwrap(),
            None
        );
    }

    #[test]
    fn always_runs_every_time() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().to_path_buf(),
            ..Paths::default()
        };
        let runners = Runners::new(paths, None);
        for _ in 0..3 {
            assert_eq!(
                runners
                    .run("thing", Frequency::Always, false, || Ok(1))
                    .unwrap(),
                Some(1)
            );
        }
    }

    #[test]
    fn a_failed_run_clears_its_marker_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().to_path_buf(),
            ..Paths::default()
        };
        std::fs::create_dir_all(paths.cpath(Lookup::Sem)).unwrap();
        let runners = Runners::new(paths.clone(), None);

        let failed =
            runners.run::<()>("kept", Frequency::Once, false, || Err("no".into()));
        assert!(failed.is_err());
        assert!(FileSemaphores::new(paths.cpath(Lookup::Sem))
            .has_run("kept", Frequency::Once));

        let failed =
            runners.run::<()>("cleared", Frequency::Once, true, || Err("no".into()));
        assert!(failed.is_err());
        assert!(!FileSemaphores::new(paths.cpath(Lookup::Sem))
            .has_run("cleared", Frequency::Once));
    }

    #[test]
    fn per_instance_needs_an_instance_id() {
        let paths = Paths::default();
        assert!(Runners::new(paths.clone(), None)
            .semaphores(Frequency::Instance)
            .is_none());
        let sem = Runners::new(paths, Some("i-1".into()))
            .semaphores(Frequency::Instance)
            .unwrap();
        assert_eq!(
            sem.path("thing", Frequency::Instance),
            Path::new("/var/lib/cloud/instances/i-1/sem/thing")
        );
    }
}
