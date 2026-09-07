//! Port of `cc_resizefs.py`: grow the root filesystem to fill its device.
//!
//! Like `cc_growpart`, this one cannot be split into "decide, then act": the
//! probe that says whether a UFS filesystem needs growing *is* a `growfs -N`
//! run, and which command to use is only known after `btrfs --version` has
//! answered. So the machine goes behind [`Host`], with [`Fixture`] answering
//! from a script and recording what was asked -- that record is what the
//! differential compares -- and [`Live`] doing the real thing.
//!
//! The resizers for UFS, ZFS and HAMMER2 are BSD; they are ported as written
//! because the filesystem type comes from the mount table and nothing stops a
//! Linux kernel from reporting one of them.

use std::path::Path;

use ci_config::{Object, Value};
use ci_log::Logger;

pub use super::growpart::CommandResult;
use super::growpart::{logexc, ProcError};
use super::{py_str, Args};

const SOURCE: &str = "cc_resizefs.py";

/// `NOBLOCK`.
const NOBLOCK: &str = "noblock";

/// `util.get_mount_info(path, get_mnt_opts=True)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mount {
    pub devpth: String,
    pub fs_type: String,
    pub mount_point: String,
    pub opts: String,
}

/// `os.stat` failing: the `ENOENT` upstream has two messages for, and
/// everything else, which it re-raises out of `handle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatError {
    NotFound,
    Other(String),
}

/// Everything `cc_resizefs` asks the machine, in upstream's own terms.
///
/// The `&mut self` is not for state but for recording: a [`Fixture`] appends
/// each question to a list, so the differential compares the *sequence* of
/// probes and not only the answer.
pub trait Host {
    /// `subp.subp(argv)`, returning stdout and stderr. A non-zero exit is the
    /// `Err`.
    fn subp(&mut self, argv: &[String]) -> Result<(String, String), ProcError>;

    /// `os.path.exists(path)`.
    fn exists(&mut self, path: &str) -> bool;

    /// `os.path.isdir(path)`.
    fn is_dir(&mut self, path: &str) -> bool;

    /// `os.stat(path).st_mode`.
    fn stat_mode(&mut self, path: &str) -> Result<u32, StatError>;

    /// `util.get_mount_info(path)`.
    fn mount_info(&mut self, path: &str) -> Option<Mount>;

    /// `util.get_mount_info(path, get_mnt_opts=True)`, which is a separate
    /// question because `util.mount_is_read_write` asks it separately.
    fn mount_opts(&mut self, path: &str) -> Option<String>;

    /// `util.is_container()`.
    fn is_container(&mut self) -> bool;

    /// `util.get_cmdline()`.
    fn cmdline(&mut self) -> String;

    /// `util.find_devs_with(criteria)`.
    fn find_devs_with(&mut self, criteria: &str) -> Vec<String>;

    /// `util.fork_cb(do_resize, resize_cmd)`.
    ///
    /// The logger is here because the port does not fork (deviation 158) and
    /// so has to write what upstream's *child* would have written: the child
    /// shares the parent's handlers, so its two `logexc` pairs land in the
    /// same `/var/log/cloud-init.log`. The caller never learns of a failure
    /// either way, which is why this returns nothing.
    fn fork_resize(&mut self, argv: &[String], log: &mut Logger);
}

/// `RESIZE_FS_PREFIXES_CMDS`, in the order the first matching prefix wins.
const RESIZE_FS_PREFIXES_CMDS: [(&str, Resizer); 7] = [
    ("btrfs", Resizer::Btrfs),
    ("ext", Resizer::Ext),
    ("xfs", Resizer::Xfs),
    ("ufs", Resizer::Ufs),
    ("zfs", Resizer::Zfs),
    ("hammer2", Resizer::Hammer2),
    ("bcachefs", Resizer::Bcachefs),
];

/// One `_resize_*` function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resizer {
    Btrfs,
    Ext,
    Xfs,
    Ufs,
    Zfs,
    Hammer2,
    Bcachefs,
}

impl Resizer {
    /// The `_resize_*` body: the argv to run, or the error it let escape.
    fn command(
        self,
        host: &mut dyn Host,
        mount_point: &str,
        devpth: &str,
    ) -> Result<Vec<String>, String> {
        Ok(match self {
            Self::Btrfs => return resize_btrfs(host, mount_point, devpth),
            Self::Ext => argv(&["resize2fs", devpth]),
            Self::Xfs => argv(&["xfs_growfs", mount_point]),
            Self::Ufs => argv(&["growfs", "-y", mount_point]),
            Self::Zfs => argv(&["zpool", "online", "-e", mount_point, devpth]),
            Self::Hammer2 => argv(&["hammer2", "growfs", mount_point]),
            Self::Bcachefs => argv(&["bcachefs", "device", "resize", devpth]),
        })
    }
}

/// `_resize_btrfs`.
fn resize_btrfs(
    host: &mut dyn Host,
    mount_point: &str,
    _devpth: &str,
) -> Result<Vec<String>, String> {
    let snapshots = format!("{mount_point}/.snapshots");
    // A read-only "/" can still be grown through a subvolume that is not, so
    // the `and` short-circuits on a writable mount and never looks.
    let target = if !mount_is_read_write(host, mount_point)? && host.is_dir(&snapshots)
    {
        snapshots
    } else {
        mount_point.to_owned()
    };
    let mut cmd = argv(&["btrfs", "filesystem", "resize", "max", &target]);

    let btrfs_with_queue = Version::from_str("5.10")?;
    let (stdout, _) = host
        .subp(&argv(&["btrfs", "--version"]))
        .map_err(|error| error.to_string())?;
    let reported = stdout
        .split('\n')
        .next()
        .unwrap_or_default()
        .rsplit('v')
        .next()
        .unwrap_or_default()
        .trim();
    let system_btrfs_ver = Version::from_str(reported)?;

    // `>=` is the namedtuple's, not the class's own `__gt__`: `functools
    // .total_ordering` fills in nothing because `tuple` already defines all
    // four. So this is a plain lexicographic compare over the four ints, and
    // it is the correct one -- `__gt__` is not. See bug B89 in
    // docs/COMPAT.md.
    if system_btrfs_ver >= btrfs_with_queue {
        // `cmd.index("resize")`, which is always there.
        cmd.insert(3, "--enqueue".to_owned());
    }
    Ok(cmd)
}

/// `lifecycle.Version`, as much of it as `_resize_btrfs` uses.
///
/// The `-1` defaults are upstream's tiebreak in favour of the more specific
/// number: `3.10` is greater than `3.9.9.9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: i64,
    minor: i64,
    patch: i64,
    rev: i64,
}

impl Version {
    /// `Version.from_str`.
    fn from_str(text: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        for part in text.split('.') {
            parts.push(py_int(part)?);
        }
        if parts.len() > 4 {
            return Err(format!(
                "Version.__new__() takes from 1 to 5 positional arguments but \
                 {} were given",
                parts.len() + 1
            ));
        }
        let at = |index: usize| parts.get(index).copied().unwrap_or(-1);
        Ok(Self {
            major: at(0),
            minor: at(1),
            patch: at(2),
            rev: at(3),
        })
    }
}

/// `int(text)` for the shapes a version segment can take: surrounding
/// whitespace, an optional sign, and underscores between digits.
fn py_int(text: &str) -> Result<i64, String> {
    let bad = || format!("invalid literal for int() with base 10: '{text}'");
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return Err(bad());
    }
    let value: i64 = digits.replace('_', "").parse().map_err(|_| bad())?;
    Ok(if negative { -value } else { value })
}

/// `util.mount_is_read_write`.
///
/// The `Err` is the `TypeError` upstream's `[-1]` raises on a mount point it
/// could not place, which nothing catches.
fn mount_is_read_write(host: &mut dyn Host, mount_point: &str) -> Result<bool, String> {
    let Some(opts) = host.mount_opts(mount_point) else {
        return Err("'NoneType' object is not subscriptable".to_owned());
    };
    Ok(opts.split(',').next() == Some("rw"))
}

/// `_can_skip_resize_ufs`.
fn can_skip_resize_ufs(host: &mut dyn Host, devpth: &str) -> Result<bool, String> {
    // growfs exits 1 for almost every failure, so the "already big enough"
    // case has to be told apart by what it printed.
    const SKIP_START: &str = "growfs: requested size";
    const SKIP_CONTAIN: &str = "is not larger than the current filesystem size";

    match host.subp(&argv(&["growfs", "-N", devpth])) {
        Ok(_) => Ok(false),
        Err(error) => {
            if error.stderr.starts_with(SKIP_START)
                && error.stderr.contains(SKIP_CONTAIN)
            {
                return Ok(true);
            }
            Err(error.to_string())
        }
    }
}

/// `can_skip_resize`: `RESIZE_FS_PRECHECK_CMDS` has the one UFS entry.
fn can_skip_resize(
    host: &mut dyn Host,
    fs_type: &str,
    devpth: &str,
) -> Result<bool, String> {
    if fs_type.to_lowercase().starts_with("ufs") {
        return can_skip_resize_ufs(host, devpth);
    }
    Ok(false)
}

/// `get_device_info_from_zpool`.
fn device_info_from_zpool(
    host: &mut dyn Host,
    zpool: &str,
    log: &mut Logger,
) -> Option<String> {
    // zpool has a 10 second timeout waiting for /dev/zfs, so a container that
    // will never have one is told at debug rather than warned at.
    let quiet = host.is_container();
    let warn = |log: &mut Logger, message: &str| {
        if quiet {
            log.debug(SOURCE, message);
        } else {
            log.warning(SOURCE, message);
        }
    };

    if !host.exists("/dev/zfs") {
        log.debug(SOURCE, "Cannot get zpool info, no /dev/zfs");
        return None;
    }
    let (zpoolstatus, err) = match host.subp(&argv(&["zpool", "status", zpool])) {
        Ok(pair) => pair,
        Err(error) => {
            warn(
                log,
                &format!("Unable to get zpool status of {zpool}: {error}"),
            );
            return None;
        }
    };
    if !err.is_empty() {
        log.info(
            SOURCE,
            &format!("zpool status returned error: [{err}] for zpool [{zpool}]"),
        );
        return None;
    }
    for line in zpoolstatus.split('\n') {
        // `re.search(r".*(ONLINE).*", line)`, which is a substring test.
        if line.contains("ONLINE") && !line.contains(zpool) && !line.contains("state") {
            let disk = line
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned();
            log.debug(SOURCE, &format!("found zpool \"{zpool}\" on disk {disk}"));
            return Some(disk);
        }
    }
    warn(
        log,
        &format!("No zpool found: [{zpool}]: out: [{zpoolstatus}] err: {err}"),
    );
    None
}

/// `maybe_get_writable_device_path`.
///
/// # Errors
/// The `os.stat` failure that is not `ENOENT`, which upstream re-raises.
fn maybe_get_writable_device_path(
    host: &mut dyn Host,
    devpath: &str,
    info: &str,
    log: &mut Logger,
) -> Result<Option<String>, String> {
    let container = host.is_container();
    let mut devpath = devpath.to_owned();

    if devpath == "/dev/root" && !host.exists(&devpath) && !container {
        let cmdline = host.cmdline();
        let Some(found) = rootdev_from_cmdline(host, &cmdline) else {
            log.warning(SOURCE, "Unable to find device '/dev/root'");
            return Ok(None);
        };
        devpath = found;
        log.debug(
            SOURCE,
            &format!("Converted /dev/root to '{devpath}' per kernel cmdline"),
        );
    }

    if devpath == "overlayroot" {
        log.debug(
            SOURCE,
            &format!("Not attempting to resize devpath '{devpath}': {info}"),
        );
        return Ok(None);
    }

    // A FreeBSD zpool can name `gpt/<label>`, which is not a path to stat.
    if devpath.starts_with("gpt/") {
        log.debug(SOURCE, "We have a gpt label - just go ahead");
        return Ok(Some(devpath));
    }
    // Or a bare name as returned by gpart, such as da0p3.
    if !devpath.starts_with("/dev/") && !host.exists(&devpath) {
        let fulldevpath = format!("/dev/{}", devpath.trim_start_matches('/'));
        log.debug(
            SOURCE,
            &format!(
                "'{devpath}' doesn't appear to be a valid device path. Trying \
                 '{fulldevpath}'"
            ),
        );
        devpath = fulldevpath;
    }

    let mode = match host.stat_mode(&devpath) {
        Ok(mode) => mode,
        Err(StatError::NotFound) => {
            if container {
                log.debug(
                    SOURCE,
                    &format!(
                        "Device '{devpath}' did not exist in container. cannot \
                         resize: {info}"
                    ),
                );
            } else {
                log.warning(
                    SOURCE,
                    &format!("Device '{devpath}' did not exist. cannot resize: {info}"),
                );
            }
            return Ok(None);
        }
        Err(StatError::Other(message)) => return Err(message),
    };

    // `stat.S_ISBLK` and `stat.S_ISCHR`.
    let kind = mode & 0o170_000;
    if kind != 0o60_000 && kind != 0o20_000 {
        if container {
            log.debug(
                SOURCE,
                &format!(
                    "device '{devpath}' not a block device in container. cannot \
                     resize: {info}"
                ),
            );
        } else {
            log.warning(
                SOURCE,
                &format!(
                    "device '{devpath}' not a block device. cannot resize: {info}"
                ),
            );
        }
        return Ok(None);
    }
    Ok(Some(devpath))
}

/// `util.rootdev_from_cmdline`, which lives in `ci-sys` because `cc_growpart`
/// calls it too; this only lends it the two probes it needs.
fn rootdev_from_cmdline(host: &mut dyn Host, cmdline: &str) -> Option<String> {
    struct Probe<'a>(&'a mut dyn Host);

    impl ci_sys::rootdev::DevProbe for Probe<'_> {
        fn exists(&mut self, path: &str) -> bool {
            self.0.exists(path)
        }

        fn find_devs_with(&mut self, criteria: &str) -> Vec<String> {
            self.0.find_devs_with(criteria)
        }
    }

    ci_sys::rootdev::rootdev_from_cmdline(&mut Probe(host), cmdline)
}

/// `do_resize`.
fn do_resize(
    host: &mut dyn Host,
    cmd: &[String],
    log: &mut Logger,
) -> Result<(), String> {
    match host.subp(cmd) {
        Ok(_) => Ok(()),
        Err(error) => {
            logexc(
                log,
                &format!("Failed to resize filesystem (cmd={})", py_tuple(cmd)),
            );
            Err(error.to_string())
        }
    }
}

/// `handle`, given a machine to run against.
///
/// # Errors
/// Whatever upstream lets escape `handle`: the failed resize, the `os.stat`
/// that was not `ENOENT`, and the type errors the version and mount helpers
/// raise.
pub fn handle_with(
    name: &str,
    cfg: &Object,
    args: &Value,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<(), String> {
    // `util.get_cfg_option_str(cfg, "resize_rootfs", True)`: the default is
    // the bool, and anything configured is `str()` of it.
    let resize_root = match args.as_array().and_then(|items| items.first()) {
        Some(value) => value.clone(),
        None => cfg
            .get("resize_rootfs")
            .map_or(Value::Bool(true), |value| Value::from(py_str(value))),
    };

    if !translate_bool_noblock(&resize_root) {
        log.debug(
            SOURCE,
            &format!("Skipping module named {name}, resizing disabled"),
        );
        return Ok(());
    }

    let mut resize_what = "/".to_owned();
    let Some(mount) = host.mount_info(&resize_what) else {
        log.warning(
            SOURCE,
            &format!("Could not determine filesystem type of {resize_what}"),
        );
        return Ok(());
    };
    let (mut devpth, fs_type, mount_point) =
        (mount.devpth, mount.fs_type, mount.mount_point);

    // For zfs the mount table names the dataset, so the pool has to be dug
    // out of it and resized instead.
    if fs_type == "zfs" {
        let zpool = devpth.split('/').next().unwrap_or_default().to_owned();
        let Some(found) = device_info_from_zpool(host, &zpool, log) else {
            return Ok(());
        };
        devpth = found;
        resize_what = zpool;
    }

    let info = format!("dev={devpth} mnt_point={mount_point} path={resize_what}");
    log.debug(SOURCE, &format!("resize_info: {info}"));

    let Some(devpth) = maybe_get_writable_device_path(host, &devpth, &info, log)?
    else {
        return Ok(());
    };

    if can_skip_resize(host, &fs_type, &devpth)? {
        log.debug(
            SOURCE,
            &format!("Skip resize filesystem type {fs_type} for {resize_what}"),
        );
        return Ok(());
    }

    let fstype_lc = fs_type.to_lowercase();
    let Some((_, resizer)) = RESIZE_FS_PREFIXES_CMDS
        .iter()
        .find(|(prefix, _)| fstype_lc.starts_with(prefix))
    else {
        log.warning(
            SOURCE,
            &format!(
                "Not resizing unknown filesystem type {fs_type} for {resize_what}"
            ),
        );
        return Ok(());
    };

    let resize_cmd = resizer.command(host, &resize_what, &devpth)?;
    log.debug(
        SOURCE,
        &format!(
            "Resizing {resize_what} ({fs_type}) using {}",
            resize_cmd.join(" ")
        ),
    );

    let noblock = resize_root.as_str() == Some(NOBLOCK);
    if noblock {
        host.fork_resize(&resize_cmd, log);
    } else {
        do_resize(host, &resize_cmd, log)?;
    }
    let action = if noblock {
        "Resizing (via forking)"
    } else {
        "Resized"
    };
    log.debug(
        SOURCE,
        &format!(
            "{action} root filesystem (type={fs_type}, val={})",
            py_str(&resize_root)
        ),
    );
    Ok(())
}

/// `handle`.
///
/// # Errors
/// Whatever upstream lets escape `handle`.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    // Nothing below is rootable: `resize2fs` and friends act on the machine's
    // own root filesystem. A rooted run is a test, and a test must not grow
    // the host's disk.
    if args.root != Path::new("/") {
        return Ok(());
    }
    let mut host = Live;
    let (name, cfg, extra) =
        (args.name.to_owned(), args.cfg.clone(), args.args.clone());
    handle_with(&name, &cfg, &extra, &mut host, &mut *args.logger)
}

/// `util.translate_bool(val, addons=[NOBLOCK])`.
fn translate_bool_noblock(value: &Value) -> bool {
    if !ci_config::option::py_truthy(value) {
        return false;
    }
    if let Value::Bool(flag) = value {
        return *flag;
    }
    matches!(
        py_str(value).to_lowercase().trim(),
        "true" | "1" | "on" | "yes" | NOBLOCK
    )
}

/// `repr()` of a tuple of strings, which is how the failed command is printed.
fn py_tuple(items: &[String]) -> String {
    let inner: Vec<String> = items
        .iter()
        .map(|item| format!("'{}'", item.replace('\\', r"\\").replace('\'', r"\'")))
        .collect();
    if inner.len() == 1 {
        return format!("({},)", inner.join(""));
    }
    format!("({})", inner.join(", "))
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// The real machine.
#[derive(Debug)]
pub struct Live;

impl Host for Live {
    fn subp(&mut self, argv: &[String]) -> Result<(String, String), ProcError> {
        let out = ci_sys::subp::Subp::new(argv)
            .run()
            .map_err(|error| ProcError {
                argv: argv.to_vec(),
                exit_code: None,
                stdout: String::new(),
                stderr: error.to_string(),
            })?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.code == Some(0) {
            return Ok((stdout, stderr));
        }
        Err(ProcError {
            argv: argv.to_vec(),
            exit_code: out.code,
            stdout,
            stderr,
        })
    }

    fn exists(&mut self, path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn is_dir(&mut self, path: &str) -> bool {
        std::fs::metadata(path).is_ok_and(|meta| meta.is_dir())
    }

    fn stat_mode(&mut self, path: &str) -> Result<u32, StatError> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .map(|meta| meta.mode())
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    StatError::NotFound
                } else {
                    StatError::Other(ci_core::pyerr::oserror(&error, Path::new(path)))
                }
            })
    }

    fn mount_info(&mut self, path: &str) -> Option<Mount> {
        ci_sys::mount::get_mount_info(path).map(|info| Mount {
            devpth: info.devpth,
            fs_type: info.fs_type,
            mount_point: info.mount_point,
            opts: info.opts,
        })
    }

    fn mount_opts(&mut self, path: &str) -> Option<String> {
        ci_sys::mount::get_mount_info(path).map(|info| info.opts)
    }

    fn is_container(&mut self) -> bool {
        ci_core::container::is_container()
    }

    fn cmdline(&mut self) -> String {
        ci_config::cmdline::get_cmdline()
    }

    fn find_devs_with(&mut self, criteria: &str) -> Vec<String> {
        ci_sys::mount::find_devs_with(Some(criteria))
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    fn fork_resize(&mut self, argv: &[String], log: &mut Logger) {
        // Upstream forks and lets the child run `do_resize`. There is nothing
        // to fork into here without `unsafe`, so the resize runs in this
        // process and the caller waits for it -- deviation 158. What the
        // child would have logged is logged here instead, because the child
        // shares the parent's handlers and its lines land in the same file:
        // `do_resize`'s own message first, then `fork_cb`'s.
        if do_resize(self, argv, log).is_err() {
            logexc(log, "Failed forking and calling callback do_resize");
        }
    }
}

/// A [`Host`] whose every answer is set up front.
///
/// This is what `dump-cc-resizefs` drives, so the differential can compare the
/// order the module probes a machine in and not only what it concluded.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Keyed by the argv joined with spaces.
    pub commands: Vec<(String, CommandResult)>,
    pub exists: Vec<String>,
    pub dirs: Vec<String>,
    /// `st_mode`, which the fixture gives in octal.
    pub stat: Vec<(String, u32)>,
    pub mounts: Vec<(String, Mount)>,
    pub container: bool,
    pub cmdline: String,
    pub devs: Vec<(String, Vec<String>)>,
    /// What the module asked, in order.
    pub calls: Vec<String>,
}

impl Fixture {
    fn record(&mut self, call: String) {
        self.calls.push(call);
    }

    fn lookup<'a, T>(table: &'a [(String, T)], key: &str) -> Option<&'a T> {
        table
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

impl Host for Fixture {
    fn subp(&mut self, argv: &[String]) -> Result<(String, String), ProcError> {
        let key = argv.join(" ");
        self.record(format!("subp {key}"));
        let Some(result) = Self::lookup(&self.commands, &key) else {
            return Err(ProcError {
                argv: argv.to_vec(),
                exit_code: Some(127),
                stdout: String::new(),
                stderr: format!(
                    "{}: not found",
                    argv.first().cloned().unwrap_or_default()
                ),
            });
        };
        if result.exit_code == 0 {
            return Ok((result.stdout.clone(), result.stderr.clone()));
        }
        Err(ProcError {
            argv: argv.to_vec(),
            exit_code: Some(result.exit_code),
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
        })
    }

    fn exists(&mut self, path: &str) -> bool {
        self.record(format!("exists {path}"));
        self.exists.iter().any(|name| name == path)
    }

    fn is_dir(&mut self, path: &str) -> bool {
        self.record(format!("isdir {path}"));
        self.dirs.iter().any(|name| name == path)
    }

    fn stat_mode(&mut self, path: &str) -> Result<u32, StatError> {
        self.record(format!("stat {path}"));
        Self::lookup(&self.stat, path)
            .copied()
            .ok_or(StatError::NotFound)
    }

    fn mount_info(&mut self, path: &str) -> Option<Mount> {
        self.record(format!("mount_info {path}"));
        Self::lookup(&self.mounts, path).cloned()
    }

    fn mount_opts(&mut self, path: &str) -> Option<String> {
        self.record(format!("mount_opts {path}"));
        Self::lookup(&self.mounts, path).map(|mount| mount.opts.clone())
    }

    fn is_container(&mut self) -> bool {
        self.record("is_container".to_owned());
        self.container
    }

    fn cmdline(&mut self) -> String {
        self.record("cmdline".to_owned());
        self.cmdline.clone()
    }

    fn find_devs_with(&mut self, criteria: &str) -> Vec<String> {
        self.record(format!("find_devs_with {criteria}"));
        Self::lookup(&self.devs, criteria)
            .cloned()
            .unwrap_or_default()
    }

    fn fork_resize(&mut self, argv: &[String], _log: &mut Logger) {
        self.record(format!("fork {}", argv.join(" ")));
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn mount(devpth: &str, fs_type: &str, opts: &str) -> Vec<(String, Mount)> {
        vec![(
            "/".to_owned(),
            Mount {
                devpth: devpth.to_owned(),
                fs_type: fs_type.to_owned(),
                mount_point: "/".to_owned(),
                opts: opts.to_owned(),
            },
        )]
    }

    fn block(path: &str) -> Vec<(String, u32)> {
        vec![(path.to_owned(), 0o60_660)]
    }

    fn commands(items: &[(&str, i32, &str, &str)]) -> Vec<(String, CommandResult)> {
        items
            .iter()
            .map(|(key, code, stdout, stderr)| {
                (
                    (*key).to_owned(),
                    CommandResult {
                        exit_code: *code,
                        stdout: (*stdout).to_owned(),
                        stderr: (*stderr).to_owned(),
                    },
                )
            })
            .collect()
    }

    fn run(
        cfg: &Value,
        fixture: Fixture,
    ) -> (Result<(), String>, Vec<String>, Vec<String>) {
        let mut host = fixture;
        let mut log = ci_log::Logger::capturing();
        let cfg = cfg.as_object().cloned().unwrap_or_default();
        let outcome = handle_with(
            "resizefs",
            &cfg,
            &Value::Array(Vec::new()),
            &mut host,
            &mut log,
        );
        (outcome, log.captured().to_vec(), host.calls)
    }

    #[test]
    fn an_ext4_root_is_grown_with_resize2fs() {
        let (outcome, log, calls) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/sda1", "ext4", "rw,relatime"),
                stat: block("/dev/sda1"),
                commands: commands(&[("resize2fs /dev/sda1", 0, "", "")]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(calls.contains(&"subp resize2fs /dev/sda1".to_owned()));
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[DEBUG]: Resizing / (ext4) using resize2fs /dev/sda1"));
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[DEBUG]: Resized root filesystem (type=ext4, val=True)"));
    }

    #[test]
    fn resize_rootfs_false_stops_before_the_mount_table_is_read() {
        let (outcome, log, calls) = run(
            &serde_json::json!({"resize_rootfs": false}),
            Fixture::default(),
        );
        assert!(outcome.is_ok());
        assert!(calls.is_empty());
        assert_eq!(
            log,
            ["cc_resizefs.py[DEBUG]: Skipping module named resizefs, resizing disabled"]
        );
    }

    #[test]
    fn noblock_forks_instead_of_waiting() {
        let (outcome, log, calls) = run(
            &serde_json::json!({"resize_rootfs": "noblock"}),
            Fixture {
                mounts: mount("/dev/sda1", "ext4", "rw"),
                stat: block("/dev/sda1"),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(calls.contains(&"fork resize2fs /dev/sda1".to_owned()));
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[DEBUG]: Resizing (via forking) root filesystem \
                (type=ext4, val=noblock)"));
    }

    #[test]
    fn a_failed_resize_is_logged_twice_and_re_raised() {
        let (outcome, log, _) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/sda1", "ext4", "rw"),
                stat: block("/dev/sda1"),
                commands: commands(&[("resize2fs /dev/sda1", 1, "", "bad magic\n")]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_err());
        assert_eq!(
            log.iter()
                .filter(|line| line.contains(
                    "Failed to resize filesystem (cmd=('resize2fs', '/dev/sda1'))"
                ))
                .count(),
            2
        );
    }

    #[test]
    fn a_read_only_btrfs_root_is_grown_through_its_snapshot_subvolume() {
        let (outcome, _, calls) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/sda1", "btrfs", "ro,relatime"),
                stat: block("/dev/sda1"),
                dirs: vec!["//.snapshots".to_owned()],
                commands: commands(&[
                    ("btrfs --version", 0, "btrfs-progs v6.2\n", ""),
                    (
                        "btrfs filesystem resize --enqueue max //.snapshots",
                        0,
                        "",
                        "",
                    ),
                ]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(calls.contains(
            &"subp btrfs filesystem resize --enqueue max //.snapshots".to_owned()
        ));
    }

    #[test]
    fn an_old_btrfs_does_not_get_the_enqueue_flag() {
        let (outcome, _, calls) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/sda1", "btrfs", "rw"),
                stat: block("/dev/sda1"),
                commands: commands(&[
                    ("btrfs --version", 0, "btrfs-progs v5.4.1\n", ""),
                    ("btrfs filesystem resize max /", 0, "", ""),
                ]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(calls.contains(&"subp btrfs filesystem resize max /".to_owned()));
    }

    #[test]
    fn dev_root_is_resolved_through_the_kernel_command_line() {
        let (outcome, log, _) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/root", "xfs", "rw"),
                stat: block("/dev/sda1"),
                cmdline: "root=/dev/sda1 ro".to_owned(),
                commands: commands(&[("xfs_growfs /", 0, "", "")]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[DEBUG]: Converted /dev/root to '/dev/sda1' per \
                kernel cmdline"));
    }

    #[test]
    fn a_device_that_is_not_a_block_device_warns_and_stops() {
        let (outcome, log, _) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/loop0", "ext4", "rw"),
                stat: vec![("/dev/loop0".to_owned(), 0o100_644)],
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(log.iter().any(|line| line.starts_with(
            "cc_resizefs.py[WARNING]: device '/dev/loop0' not a block device."
        )));
    }

    #[test]
    fn an_unknown_filesystem_type_is_left_alone() {
        let (outcome, log, _) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/sda1", "reiserfs", "rw"),
                stat: block("/dev/sda1"),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[WARNING]: Not resizing unknown filesystem type \
                reiserfs for /"));
    }

    #[test]
    fn a_ufs_root_that_is_already_big_enough_is_skipped() {
        let (outcome, log, _) = run(
            &serde_json::json!({}),
            Fixture {
                mounts: mount("/dev/da0p3", "ufs", "rw"),
                stat: block("/dev/da0p3"),
                commands: commands(&[(
                    "growfs -N /dev/da0p3",
                    1,
                    "",
                    "growfs: requested size 4.0GB is not larger than the current \
                     filesystem size 4.0GB\n",
                )]),
                ..Fixture::default()
            },
        );
        assert!(outcome.is_ok());
        assert!(log.iter().any(|line| line
            == "cc_resizefs.py[DEBUG]: Skip resize filesystem type ufs for /"));
    }

    #[test]
    fn a_version_compares_lexicographically_with_a_missing_field_as_minus_one() {
        // `>=` is `tuple.__ge__`, so a larger minor under a smaller major does
        // not win, and a version with a patch beats one without.
        assert!(
            Version::from_str("5.10").unwrap() >= Version::from_str("5.10").unwrap()
        );
        assert!(
            Version::from_str("6.2").unwrap() >= Version::from_str("5.10").unwrap()
        );
        assert!(
            Version::from_str("5.10.0").unwrap() >= Version::from_str("5.10").unwrap()
        );
        assert!(
            Version::from_str("4.20").unwrap() < Version::from_str("5.10").unwrap()
        );
        assert!(
            Version::from_str("5.4.1").unwrap() < Version::from_str("5.10").unwrap()
        );
        assert_eq!(
            Version::from_str("6.x").unwrap_err(),
            "invalid literal for int() with base 10: 'x'"
        );
    }
}
