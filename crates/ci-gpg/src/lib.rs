//! `cloudinit/gpg.py` — fetching, converting and deleting gpg keys.
//!
//! Upstream models this as a context manager owning a throwaway `GNUPGHOME`,
//! which it wipes on exit after killing the agents gpg spawns behind it.
//! [`RealGpg`] keeps that shape and cleans up on drop.
//!
//! The trait exists because [`crate::Gpg`] is the seam the apt module is
//! written against: upstream threads a live `GPG` instance through
//! `add_apt_key` and friends, so tests and the differential dumps can hand it
//! a recorded stand-in instead of shelling out to the real binary.

use std::thread::sleep;
use std::time::Duration;

use ci_log::Logger;
use ci_sys::subp;

/// `gpg.py:22`
const HOME: &str = "GNUPGHOME";

/// `cc_apt_configure.py:58`
pub const DEFAULT_KEYSERVER: &str = "keyserver.ubuntu.com";

/// `gpg.py:103` — `retries=(1, 1)`, so three attempts in all.
const DEFAULT_RETRIES: &[u64] = &[1, 1];

const SOURCE: &str = "gpg.py";

/// The gpg operations `cc_apt_configure` needs.
pub trait Gpg {
    /// `gpg --export --armour`, or `None` when the key is not held locally.
    fn export_armour(&mut self, key: &str, log: &mut Logger) -> Option<String>;

    /// `gpg --dearmor`, returning the binary keyring.
    fn dearmor(&mut self, key: &str) -> Result<Vec<u8>, String>;

    /// Fingerprints held in `key_file`.
    fn list_keys(
        &mut self,
        key_file: &str,
        human_output: bool,
        log: &mut Logger,
    ) -> Result<String, String>;

    /// Import `key` from `keyserver`, retrying because keyservers are flaky.
    fn recv_key(
        &mut self,
        key: &str,
        keyserver: &str,
        log: &mut Logger,
    ) -> Result<(), String>;

    /// Drop `key` from the local ring. Failure is logged, never raised.
    fn delete_key(&mut self, key: &str, log: &mut Logger);

    /// `gpg.py:165` — export the key, falling back to a keyserver fetch.
    ///
    /// The imported key is deleted again either way, so the ring is left as it
    /// was found.
    fn getkeybyid(
        &mut self,
        keyid: &str,
        keyserver: &str,
        log: &mut Logger,
    ) -> Result<Option<String>, String> {
        let armour = self.export_armour(keyid, log);
        // `if not armour` — an empty export counts as a miss, not a hit.
        if !armour.as_deref().is_none_or(str::is_empty) {
            return Ok(armour);
        }
        let fetched = match self.recv_key(keyid, keyserver, log) {
            Ok(()) => Ok(self.export_armour(keyid, log)),
            Err(err) => {
                log.error(SOURCE, &format!("Failed to obtain gpg key {keyid}"));
                Err(err)
            }
        };
        // upstream's `finally`, so it runs on the failed path too.
        self.delete_key(keyid, log);
        fetched
    }
}

/// The real thing: a temporary `GNUPGHOME` and the `gpg` binary.
#[derive(Debug)]
pub struct RealGpg {
    temp_dir: Option<tempfile::TempDir>,
    /// Upstream sets this lazily on first `env` use, and `kill_gpg` returns
    /// early when it is unset, so a `GPG()` that never ran gpg kills nothing.
    gpg_started: bool,
}

impl RealGpg {
    pub fn new() -> Result<Self, String> {
        let temp_dir = tempfile::TempDir::new()
            .map_err(|err| format!("failed to create gpg home: {err}"))?;
        Ok(Self {
            temp_dir: Some(temp_dir),
            gpg_started: false,
        })
    }

    /// `gpg.py:33` — the lazy `env` property.
    fn command(&mut self, argv: Vec<String>) -> subp::Subp {
        self.gpg_started = true;
        let home = self
            .temp_dir
            .as_ref()
            .map(|dir| dir.path().display().to_string());
        let cmd = subp::Subp::new(argv);
        match home {
            Some(home) => cmd.env(HOME, home),
            None => cmd,
        }
    }

    /// `gpg.py:186` — stop the daemons gpg leaves behind (GH: 4344).
    fn kill_gpg(&mut self, log: &mut Logger) {
        if !self.gpg_started {
            return;
        }
        if subp::which("gpgconf").is_some() {
            let cmd = self.command(vec![
                "gpgconf".to_owned(),
                "--kill".to_owned(),
                "all".to_owned(),
            ]);
            if let Err(err) = cmd.check() {
                log.warning(SOURCE, &format!("Failed to clean up gpg process: {err}"));
            }
            return;
        }
        kill_stray_agents(log);
    }
}

fn kill_stray_agents(log: &mut Logger) {
    let listing = subp::Subp::new(vec![
        "ps",
        "-o",
        "ppid,pid",
        "-C",
        "keyboxd",
        "-C",
        "dirmngr",
        "-C",
        "gpg-agent",
    ])
    .run();
    let output = match listing {
        // upstream passes `rcs=[0, 1]`: 1 just means nothing matched.
        Ok(output) if matches!(output.code, Some(0 | 1)) => output,
        Ok(_) | Err(_) => {
            log.warning(SOURCE, "Failed to clean up gpg process: ps failed");
            return;
        }
    };
    let text = output.stdout_lossy();
    let Ok(pattern) = regex::Regex::new(r"(\d+)\s+(\d+)") else {
        return;
    };
    let orphans: Vec<&str> = pattern
        .captures_iter(&text)
        .filter(|caps| caps.get(1).map(|m| m.as_str()) == Some("1"))
        .filter_map(|caps| caps.get(2).map(|m| m.as_str()))
        .collect();
    if orphans.is_empty() {
        return;
    }
    log.debug(
        SOURCE,
        &format!("Killing gpg-agent and dirmngr pids: {orphans:?}"),
    );
    // Upstream calls `os.kill`; this tree forbids `unsafe` and libc, so the
    // signal goes out through kill(1) instead. See COMPAT.md deviation 133.
    let mut argv = vec!["kill".to_owned(), "-KILL".to_owned()];
    argv.extend(orphans.iter().map(|pid| (*pid).to_owned()));
    if let Err(err) = subp::Subp::new(argv).check() {
        log.warning(SOURCE, &format!("Failed to clean up gpg process: {err}"));
    }
}

impl Drop for RealGpg {
    fn drop(&mut self) {
        let mut log = Logger::silent();
        self.kill_gpg(&mut log);
        self.temp_dir = None;
    }
}

impl Gpg for RealGpg {
    fn export_armour(&mut self, key: &str, log: &mut Logger) -> Option<String> {
        let cmd = self.command(vec![
            "gpg".to_owned(),
            "--export".to_owned(),
            "--armour".to_owned(),
            key.to_owned(),
        ]);
        match cmd.check() {
            Ok(output) => Some(output.stdout_lossy().into_owned()),
            Err(err) => {
                // debug, not warning: a key the system has never seen lands here.
                log.debug(
                    SOURCE,
                    &format!("Failed to export armoured key \"{key}\": {err}"),
                );
                None
            }
        }
    }

    fn dearmor(&mut self, key: &str) -> Result<Vec<u8>, String> {
        let cmd = self
            .command(vec!["gpg".to_owned(), "--dearmor".to_owned()])
            .stdin(key.as_bytes().to_vec());
        cmd.check()
            .map(|output| output.stdout)
            .map_err(|err| err.to_string())
    }

    fn list_keys(
        &mut self,
        key_file: &str,
        human_output: bool,
        log: &mut Logger,
    ) -> Result<String, String> {
        let mut argv: Vec<String> = [
            "gpg",
            "--no-options",
            "--with-fingerprint",
            "--no-default-keyring",
            "--list-keys",
            "--keyring",
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        // Upstream appends `--with-colons` *after* `--keyring`, so the keyring
        // name becomes the flag and the file lands as a search pattern.
        // Reproduced deliberately; see COMPAT.md bug B75.
        if !human_output {
            argv.push("--with-colons".to_owned());
        }
        argv.push(key_file.to_owned());

        let output = self.command(argv).check().map_err(|err| err.to_string())?;
        if !output.stderr.is_empty() {
            // Upstream's message names the wrong operation. Kept verbatim.
            log.warning(
                SOURCE,
                &format!(
                    "Failed to export armoured key \"{key_file}\": {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            );
        }
        Ok(output.stdout_lossy().into_owned())
    }

    fn recv_key(
        &mut self,
        key: &str,
        keyserver: &str,
        log: &mut Logger,
    ) -> Result<(), String> {
        log.debug(
            SOURCE,
            &format!("Importing key '{key}' from keyserver '{keyserver}'"),
        );
        let mut trynum = 0;
        let mut sleeps = DEFAULT_RETRIES.iter();
        loop {
            trynum += 1;
            let cmd = self.command(vec![
                "gpg".to_owned(),
                "--no-tty".to_owned(),
                format!("--keyserver={keyserver}"),
                "--recv-keys".to_owned(),
                key.to_owned(),
            ]);
            let Err(error) = cmd.check() else {
                log.debug(
                    SOURCE,
                    &format!(
                        "Imported key '{key}' from keyserver '{keyserver}' on try {trynum}"
                    ),
                );
                return Ok(());
            };
            match sleeps.next() {
                Some(naplen) => {
                    log.debug(
                        SOURCE,
                        &format!(
                            "Import failed with exit code {}, will try again in {naplen}s",
                            exit_code(&error)
                        ),
                    );
                    sleep(Duration::from_secs(*naplen));
                }
                None => {
                    return Err(format!(
                        "Failed to import key '{key}' from keyserver '{keyserver}' \
                         after {trynum} tries: {error}"
                    ));
                }
            }
        }
    }

    fn delete_key(&mut self, key: &str, log: &mut Logger) {
        let cmd = self.command(vec![
            "gpg".to_owned(),
            "--batch".to_owned(),
            "--yes".to_owned(),
            "--delete-keys".to_owned(),
            key.to_owned(),
        ]);
        if let Err(err) = cmd.check() {
            log.warning(SOURCE, &format!("Failed delete key \"{key}\": {err}"));
        }
    }
}

fn exit_code(err: &subp::Error) -> String {
    match err {
        subp::Error::NonZeroExit { status, .. } => status.to_string(),
        _ => "-".to_owned(),
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

    /// Records what was asked of it and replays canned exports.
    #[derive(Debug, Default)]
    struct FakeGpg {
        calls: Vec<String>,
        exports: Vec<Option<String>>,
        recv_fails: bool,
    }

    impl Gpg for FakeGpg {
        fn export_armour(&mut self, key: &str, _log: &mut Logger) -> Option<String> {
            self.calls.push(format!("export {key}"));
            if self.exports.is_empty() {
                return None;
            }
            self.exports.remove(0)
        }

        fn dearmor(&mut self, key: &str) -> Result<Vec<u8>, String> {
            self.calls.push(format!("dearmor {key}"));
            Ok(key.as_bytes().to_vec())
        }

        fn list_keys(
            &mut self,
            key_file: &str,
            _human_output: bool,
            _log: &mut Logger,
        ) -> Result<String, String> {
            self.calls.push(format!("list {key_file}"));
            Ok(String::new())
        }

        fn recv_key(
            &mut self,
            key: &str,
            keyserver: &str,
            _log: &mut Logger,
        ) -> Result<(), String> {
            self.calls.push(format!("recv {key} from {keyserver}"));
            if self.recv_fails {
                return Err(format!("Failed to import key '{key}'"));
            }
            Ok(())
        }

        fn delete_key(&mut self, key: &str, _log: &mut Logger) {
            self.calls.push(format!("delete {key}"));
        }
    }

    fn run(fake: &mut FakeGpg) -> Result<Option<String>, String> {
        let mut log = Logger::silent();
        fake.getkeybyid("ABC", DEFAULT_KEYSERVER, &mut log)
    }

    #[test]
    fn a_locally_held_key_is_returned_without_touching_the_keyserver() {
        let mut fake = FakeGpg {
            exports: vec![Some("-----BEGIN-----".to_owned())],
            ..FakeGpg::default()
        };
        assert_eq!(run(&mut fake).unwrap().as_deref(), Some("-----BEGIN-----"));
        assert_eq!(fake.calls, vec!["export ABC"]);
    }

    #[test]
    fn a_missing_key_is_fetched_and_then_deleted_again() {
        let mut fake = FakeGpg {
            exports: vec![None, Some("fetched".to_owned())],
            ..FakeGpg::default()
        };
        assert_eq!(run(&mut fake).unwrap().as_deref(), Some("fetched"));
        assert_eq!(
            fake.calls,
            vec![
                "export ABC",
                "recv ABC from keyserver.ubuntu.com",
                "export ABC",
                "delete ABC",
            ]
        );
    }

    /// `if not armour` is falsy for `""`, not just for `None`.
    #[test]
    fn an_empty_export_counts_as_a_miss() {
        let mut fake = FakeGpg {
            exports: vec![Some(String::new()), Some("fetched".to_owned())],
            ..FakeGpg::default()
        };
        assert_eq!(run(&mut fake).unwrap().as_deref(), Some("fetched"));
        assert!(fake
            .calls
            .contains(&"recv ABC from keyserver.ubuntu.com".to_owned()));
    }

    /// The `finally` runs on the failed path too, so the ring is left clean.
    #[test]
    fn a_keyserver_failure_still_deletes_the_key() {
        let mut fake = FakeGpg {
            exports: vec![None],
            recv_fails: true,
            ..FakeGpg::default()
        };
        assert!(run(&mut fake).is_err());
        assert_eq!(fake.calls.last().map(String::as_str), Some("delete ABC"));
    }
}
