//! `cloud-init-generator`, the systemd generator that decides whether
//! `cloud-init.target` is pulled into the boot.
//!
//! Like [`ds_identify`](../ds_identify/index.html), the upstream reference is a
//! POSIX shell script rather than Python:
//! `/usr/lib/systemd/system-generators/cloud-init-generator`. It runs
//! `ds-identify`, turns its exit code into a symlink under
//! `<early-dir>/multi-user.target.wants/`, and drops an `enabled` or `disabled`
//! flag file in `/run/cloud-init` for the units to read.
//!
//! This is a transliteration, including two behaviours that are defects (see
//! `docs/COMPAT.md` B56 and B57): the "no ds-identify, fail open" branch is
//! dead because the script forgets to return, and the `/dev/kmsg` logging
//! fallback is unreachable because a redirection failure on the special
//! builtin `:` makes `dash` exit. Both are reproduced, because the decision
//! this program makes is whether a machine provisions at all, and the port is
//! not the place to start guessing differently from the script that ships.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use ci_sys::subp::Subp;

/// A redirection failure on a POSIX *special* builtin (`:` here) terminates a
/// non-interactive shell. Carrying it as an error is how the port reproduces
/// the script dying mid-decision.
#[derive(Debug, Clone, Copy)]
pub struct ShellExit(pub i32);

/// The script's constants. Not configurable there and not configurable here:
/// a generator that took its paths from the environment would let anything
/// that can set a variable in PID 1's environment redirect the decision.
#[derive(Debug, Clone)]
pub struct Config {
    pub log_dir: PathBuf,
    pub log_file: PathBuf,
    pub enabled_file: PathBuf,
    pub disabled_file: PathBuf,
    pub cloud_system_target: PathBuf,
    pub ds_identify: PathBuf,
    pub debug_level: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self::system()
    }
}

impl Config {
    #[must_use]
    pub fn system() -> Self {
        Self {
            log_dir: PathBuf::from("/run/cloud-init"),
            log_file: PathBuf::from("/run/cloud-init/cloud-init-generator.log"),
            enabled_file: PathBuf::from("/run/cloud-init/enabled"),
            disabled_file: PathBuf::from("/run/cloud-init/disabled"),
            cloud_system_target: PathBuf::from("/lib/systemd/system/cloud-init.target"),
            ds_identify: PathBuf::from("/usr/lib/cloud-init/ds-identify"),
            debug_level: 1,
        }
    }
}

/// `debug()`: opens the log lazily on the first message that passes the level
/// check, then appends to it.
#[derive(Debug)]
pub struct Log {
    level: i32,
    path: Option<PathBuf>,
}

impl Log {
    #[must_use]
    pub fn new(level: i32) -> Self {
        Self { level, path: None }
    }

    /// Where the log ended up, once opened. `None` until the first message.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn debug(
        &mut self,
        cfg: &Config,
        level: i32,
        msg: &str,
    ) -> Result<(), ShellExit> {
        if level > self.level {
            return Ok(());
        }
        if self.path.is_none() {
            self.path = Some(Self::open(cfg)?);
        }
        if let Some(path) = &self.path {
            // `echo >> $LOG` is a regular builtin, so a failure here is not
            // fatal and not reported: on a system where the fallback is taken
            // and /dev/kmsg is unwritable, the log is simply lost.
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
                let _ = writeln!(f, "{msg}");
            }
        }
        Ok(())
    }

    fn open(cfg: &Config) -> Result<PathBuf, ShellExit> {
        let have_dir =
            cfg.log_dir.is_dir() || std::fs::create_dir_all(&cfg.log_dir).is_ok();
        if !have_dir {
            // `{ ... } && ...` short-circuits past the `:` redirection, so this
            // is the one path on which the /dev/kmsg fallback is reachable.
            return Ok(PathBuf::from("/dev/kmsg"));
        }
        if std::fs::File::create(&cfg.log_file).is_ok() {
            return Ok(cfg.log_file.clone());
        }
        // `{ : > "$LOG_F"; } >/dev/null 2>&1` — dash exits 2 rather than
        // reaching the `|| LOG="/dev/kmsg"` the author wrote (B57).
        Err(ShellExit(2))
    }
}

/// `main()`: `normal_d`, `early_d`, `late_d` are the three directories systemd
/// passes every generator.
pub fn run(cfg: &Config, argv0: &str, args: &[String]) -> i32 {
    let mut log = Log::new(cfg.debug_level);
    match main_inner(cfg, &mut log, argv0, args) {
        Ok(code) | Err(ShellExit(code)) => code,
    }
}

fn main_inner(
    cfg: &Config,
    log: &mut Log,
    argv0: &str,
    args: &[String],
) -> Result<i32, ShellExit> {
    let arg = |n: usize| args.get(n).map_or("", String::as_str);
    let (normal_d, early_d, late_d) = (arg(0), arg(1), arg(2));
    let link_dir = Path::new(early_d).join("multi-user.target.wants");
    let link_path = link_dir.join("cloud-init.target");

    log.debug(
        cfg,
        1,
        &format!("{argv0} normal={normal_d} early={early_d} late={late_d}"),
    )?;
    log.debug(cfg, 2, &format!("{argv0} {}", args.join(" ")))?;
    log.debug(cfg, 1, "checking for datasource")?;

    if !is_executable(&cfg.ds_identify) {
        log.debug(
            cfg,
            1,
            &format!("no ds-identify in {}", cfg.ds_identify.display()),
        )?;
        // Upstream sets ds=0 here and forgets to return, so the value is
        // overwritten by the run below and cloud-init is never failed open
        // (B56). Reproduced by not returning either.
    }
    let ds = run_ds_identify(&cfg.ds_identify);
    log.debug(cfg, 1, &format!("ds-identify rc={ds}"))?;

    let mut ret: Option<i32> = None;
    match ds {
        1 | 2 => disable(cfg, log, &link_path, ds, &mut ret)?,
        0 => enable(cfg, log, &link_dir, &link_path, &mut ret)?,
        _ => {
            // `$result` is never assigned anywhere in the script, so the
            // quotes are always empty.
            log.debug(cfg, 0, &format!("unexpected result '' 'ds={ds}'"))?;
            ret = Some(3);
        }
    }

    // `return $ret` with an empty `ret` is a bare `return`, which yields the
    // status of the last command — the `:` redirection, which either succeeded
    // or already took the shell down.
    Ok(ret.unwrap_or(0))
}

fn disable(
    cfg: &Config,
    log: &mut Log,
    link_path: &Path,
    ds: i32,
    ret: &mut Option<i32>,
) -> Result<(), ShellExit> {
    if ds == 1 {
        log.debug(
            cfg,
            1,
            "cloud-init is enabled but no datasource found, disabling",
        )?;
    } else {
        log.debug(
            cfg,
            1,
            "cloud-init is disabled by kernel command line or etc_file",
        )?;
    }
    // `[ -f ]` follows the symlink, so a link whose target is missing reads as
    // "already disabled" and is left in place.
    if link_path.is_file() {
        if remove_force(link_path) {
            log.debug(
                cfg,
                1,
                &format!("disabled. removed existing {}", link_path.display()),
            )?;
        } else {
            *ret = Some(1);
            log.debug(
                cfg,
                0,
                &format!("[1] disable failed, remove {}", link_path.display()),
            )?;
        }
    } else {
        log.debug(
            cfg,
            1,
            &format!(
                "already disabled: no change needed [no {}]",
                link_path.display()
            ),
        )?;
    }
    if exists(&cfg.enabled_file) {
        log.debug(
            cfg,
            1,
            &format!(
                "removing {} and creating {}",
                cfg.enabled_file.display(),
                cfg.disabled_file.display()
            ),
        )?;
        remove_force(&cfg.enabled_file);
    }
    truncate_create(&cfg.disabled_file)
}

fn enable(
    cfg: &Config,
    log: &mut Log,
    link_dir: &Path,
    link_path: &Path,
    ret: &mut Option<i32>,
) -> Result<(), ShellExit> {
    if exists(link_path) {
        log.debug(cfg, 1, "already enabled: no change needed")?;
    } else {
        if !link_dir.is_dir() && std::fs::create_dir_all(link_dir).is_err() {
            // The message names the link, not the directory it failed to make;
            // upstream's wording is kept.
            log.debug(
                cfg,
                0,
                &format!("failed to make dir {}", link_path.display()),
            )?;
        }
        if force_symlink(&cfg.cloud_system_target, link_path) {
            log.debug(
                cfg,
                1,
                &format!(
                    "enabled via {} -> {}",
                    link_path.display(),
                    cfg.cloud_system_target.display()
                ),
            )?;
        } else {
            *ret = Some(1);
            log.debug(
                cfg,
                0,
                &format!(
                    "[1] enable failed: ln {} {}",
                    cfg.cloud_system_target.display(),
                    link_path.display()
                ),
            )?;
        }
    }
    if exists(&cfg.disabled_file) {
        log.debug(
            cfg,
            1,
            &format!(
                "removing {} and creating {}",
                cfg.disabled_file.display(),
                cfg.enabled_file.display()
            ),
        )?;
        remove_force(&cfg.disabled_file);
    }
    truncate_create(&cfg.enabled_file)
}

/// `$dsidentify` with no redirection: its stdout and stderr are the
/// generator's, which is to say the journal's.
fn run_ds_identify(path: &Path) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    match Subp::new([path]).inherit_env().timeout(None).passthrough() {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        // What a shell reports when `exec` fails: 126 for a file it may not
        // run, 127 for one that is not there.
        Err(_) if path.exists() => 126,
        Err(_) => 127,
    }
}

/// `test -x`.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// `test -e`: follows symlinks, so a dangling link does not exist.
fn exists(path: &Path) -> bool {
    path.metadata().is_ok()
}

/// `rm -f`: a path that is already gone is success.
fn remove_force(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// `ln -snf target link`.
fn force_symlink(target: &Path, link: &Path) -> bool {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// `: > file`, with the dash exit-on-redirection-failure rule.
fn truncate_create(path: &Path) -> Result<(), ShellExit> {
    if std::fs::File::create(path).is_ok() {
        Ok(())
    } else {
        Err(ShellExit(2))
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
    use ci_sys::path::TempDir;

    /// The fixture writes `ds-identify` and then execs it. A sibling test that
    /// forks in between inherits the still-open write fd, so the exec fails
    /// with `ETXTBSY` and `run` reports 3; one lock per binary closes the race.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fixture(dir: &Path, ds_rc: i32) -> Config {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("ds-identify");
        std::fs::write(&bin, format!("#!/bin/sh\nexit {ds_rc}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("cloud-init.target"), "[Unit]\n").unwrap();
        Config {
            log_dir: dir.join("run"),
            log_file: dir.join("run/cloud-init-generator.log"),
            enabled_file: dir.join("run/enabled"),
            disabled_file: dir.join("run/disabled"),
            cloud_system_target: dir.join("cloud-init.target"),
            ds_identify: bin,
            debug_level: 1,
        }
    }

    fn generator_dirs(dir: &Path) -> Vec<String> {
        ["normal", "early", "late"]
            .iter()
            .map(|n| {
                let p = dir.join(n);
                std::fs::create_dir_all(&p).unwrap();
                p.to_string_lossy().into_owned()
            })
            .collect()
    }

    fn log_of(cfg: &Config) -> String {
        std::fs::read_to_string(&cfg.log_file).unwrap()
    }

    #[test]
    fn rc_zero_links_the_target_and_writes_the_enabled_flag() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-enable").unwrap();
        let cfg = fixture(tmp.path(), 0);
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 0);

        let link = tmp
            .path()
            .join("early/multi-user.target.wants/cloud-init.target");
        assert_eq!(std::fs::read_link(&link).unwrap(), cfg.cloud_system_target);
        assert!(cfg.enabled_file.exists());
        assert!(!cfg.disabled_file.exists());
        assert!(log_of(&cfg).contains("enabled via "));
    }

    #[test]
    fn a_second_run_leaves_the_existing_link_alone() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-again").unwrap();
        let cfg = fixture(tmp.path(), 0);
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 0);
        assert_eq!(run(&cfg, "gen", &args), 0);
        assert!(log_of(&cfg).contains("already enabled: no change needed"));
    }

    #[test]
    fn rc_one_removes_the_link_and_writes_the_disabled_flag() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-disable").unwrap();
        let args = generator_dirs(tmp.path());

        let enable = fixture(tmp.path(), 0);
        assert_eq!(run(&enable, "gen", &args), 0);

        let cfg = fixture(tmp.path(), 1);
        assert_eq!(run(&cfg, "gen", &args), 0);
        let link = tmp
            .path()
            .join("early/multi-user.target.wants/cloud-init.target");
        assert!(!link.exists());
        assert!(cfg.disabled_file.exists());
        assert!(!cfg.enabled_file.exists());
        let log = log_of(&cfg);
        assert!(
            log.contains("cloud-init is enabled but no datasource found, disabling")
        );
        assert!(log.contains("disabled. removed existing "));
    }

    #[test]
    fn rc_two_says_it_was_disabled_deliberately() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-off").unwrap();
        let cfg = fixture(tmp.path(), 2);
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 0);
        let log = log_of(&cfg);
        assert!(
            log.contains("cloud-init is disabled by kernel command line or etc_file")
        );
        assert!(log.contains("already disabled: no change needed [no "));
    }

    #[test]
    fn an_unexpected_rc_makes_no_decision_at_all() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-odd").unwrap();
        let cfg = fixture(tmp.path(), 9);
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 3);
        let link = tmp
            .path()
            .join("early/multi-user.target.wants/cloud-init.target");
        assert!(!link.exists());
        assert!(!cfg.enabled_file.exists());
        assert!(!cfg.disabled_file.exists());
        assert!(log_of(&cfg).contains("unexpected result '' 'ds=9'"));
    }

    /// B56: the "fail open" branch logs, then is overruled by the run it was
    /// supposed to replace.
    #[test]
    fn a_missing_ds_identify_is_not_failed_open() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-nods").unwrap();
        let mut cfg = fixture(tmp.path(), 0);
        cfg.ds_identify = tmp.path().join("absent");
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 3);
        let log = log_of(&cfg);
        assert!(log.contains("no ds-identify in "));
        assert!(log.contains("ds-identify rc=127"));
        assert!(log.contains("unexpected result '' 'ds=127'"));
    }

    /// B57: an unwritable log file takes the whole generator down before it
    /// looks at anything.
    #[test]
    fn an_unwritable_log_file_exits_two_without_deciding() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-nolog").unwrap();
        let cfg = fixture(tmp.path(), 0);
        std::fs::create_dir_all(&cfg.log_file).unwrap();
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 2);
        let link = tmp
            .path()
            .join("early/multi-user.target.wants/cloud-init.target");
        assert!(!link.exists());
    }

    /// The one path on which the fallback the author wrote is reachable.
    #[test]
    fn an_unusable_log_dir_falls_back_to_kmsg() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-kmsg").unwrap();
        let mut cfg = fixture(tmp.path(), 0);
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        std::fs::write(tmp.path().join("blocker"), "").unwrap();
        cfg.log_dir = tmp.path().join("blocker/run");
        cfg.log_file = cfg.log_dir.join("gen.log");
        let args = generator_dirs(tmp.path());
        assert_eq!(run(&cfg, "gen", &args), 0);
        let link = tmp
            .path()
            .join("early/multi-user.target.wants/cloud-init.target");
        assert!(link.exists());
        assert!(cfg.enabled_file.exists());
    }

    #[test]
    fn level_two_messages_never_open_the_log() {
        let _lock = serialized();
        let tmp = TempDir::new(std::env::temp_dir(), "gen-lvl").unwrap();
        let cfg = fixture(tmp.path(), 0);
        let mut log = Log::new(1);
        log.debug(&cfg, 2, "quiet").unwrap();
        assert!(log.path().is_none());
        assert!(!cfg.log_file.exists());
    }
}
