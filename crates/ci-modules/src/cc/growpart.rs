//! Port of `cc_growpart.py`: grow the partition a filesystem sits on.
//!
//! Every other ported module decides first and acts second, so that a test can
//! read the decision without a machine. This one cannot be split that way:
//! `growpart --dry-run` exiting 1 *is* the decision that nothing needs doing,
//! and which partition to resize is only known after `dmsetup` and
//! `/sys/class/block` have been asked. Planning and doing are the same walk.
//!
//! So the machine goes behind a trait instead. [`Host`] is every syscall and
//! every command the upstream module makes, in the order it makes them;
//! [`Fixture`] answers them from a script and records what was asked, which is
//! what the differential compares. [`Live`] is the real thing, and `handle`
//! builds one.
//!
//! `ResizeGrowFS` and `ResizeGpart` are FreeBSD resizers. Their availability
//! probes are ported, because `mode: auto` runs them on Linux too and their
//! answers decide which resizer is picked; their `resize` bodies are ported as
//! written, but the `growfs` one needs a `manage_service` action no Linux
//! distro in `ci-distro` has, so reaching it is an error rather than a
//! service start. See docs/COMPAT.md.

use std::path::Path;

use ci_config::{type_name, Object, Value};
use ci_log::Logger;

use super::{py_str, Args};

const SOURCE: &str = "cc_growpart.py";

/// `KEYDATA_PATH`.
const KEYDATA_PATH: &str = "/cc_growpart_keydata";

/// `RESIZERS`, in the order `mode: auto` tries them.
const RESIZERS: [(&str, Resizer); 3] = [
    ("growpart", Resizer::GrowPart),
    ("growfs", Resizer::GrowFS),
    ("gpart", Resizer::Gpart),
];

/// `DEFAULT_CONFIG`.
fn default_config() -> Object {
    let mut cfg = Object::new();
    cfg.insert("mode".to_owned(), Value::from("auto"));
    cfg.insert("devices".to_owned(), Value::Array(vec![Value::from("/")]));
    cfg.insert("ignore_growroot_disabled".to_owned(), Value::Bool(false));
    cfg
}

/// `subp.ProcessExecutionError`, as much of it as this module reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcError {
    /// The `cmd=` the exception carries, which is the argv list.
    pub argv: Vec<String>,
    /// `None` for a child that never got an exit code, which upstream prints
    /// as `-` and compares unequal to every number.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl std::fmt::Display for ProcError {
    /// `MESSAGE_TMPL` with `description` and `reason` at their defaults, which
    /// is how every `subp` failure in this module is built.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Unexpected error while running command.\n\
             Command: {}\n\
             Exit code: {}\n\
             Reason: -\n\
             Stdout: {}\n\
             Stderr: {}",
            py_list(&self.argv),
            self.exit_code
                .map_or_else(|| "-".to_owned(), |code| code.to_string()),
            indent_text(&self.stdout),
            indent_text(&self.stderr),
        )
    }
}

/// `ProcessExecutionError._indent_text`, with the empty stream left empty:
/// `subp` hands over `""` rather than `None`, so the `-` placeholder is only
/// ever seen for a stream that was never captured.
pub(crate) fn indent_text(text: &str) -> String {
    text.trim_end_matches('\n').replace('\n', "\n        ")
}

/// `repr()` of a list of strings.
pub(crate) fn py_list(items: &[String]) -> String {
    let inner: Vec<String> =
        items.iter().map(|item| ci_config::repr_str(item)).collect();
    format!("[{}]", inner.join(", "))
}

/// What `os.open` did when it did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// `FileNotFoundError`, the one `get_size` catches.
    NotFound,
    /// Anything else, which escapes `handle` uncaught.
    Other(String),
}

/// `util.get_mount_info`'s first three fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    pub devpth: String,
    pub fs_type: String,
    pub mount_point: String,
}

/// Everything `cc_growpart` asks the machine, in upstream's own terms.
///
/// The `&mut self` is not for state but for recording: a [`Fixture`] appends
/// each question to a list, so the differential compares the *sequence* of
/// probes and not only the answer.
pub trait Host {
    /// `subp.subp(argv, update_env=env, data=data)`, returning stdout and
    /// stderr. A non-zero exit is the `Err`.
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(&str, &str)],
        data: Option<&[u8]>,
    ) -> Result<(String, String), ProcError>;

    /// `subp.which(program) is not None`.
    fn which(&mut self, program: &str) -> bool;

    /// `os.path.exists(path)`.
    fn exists(&mut self, path: &str) -> bool;

    /// `os.path.isfile(path)`.
    fn is_file(&mut self, path: &str) -> bool;

    /// `os.path.realpath(path)`.
    fn realpath(&mut self, path: &str) -> String;

    /// `os.stat(path).st_mode`; the `Err` is `str()` of the `OSError`.
    fn stat_mode(&mut self, path: &str) -> Result<u32, String>;

    /// `os.open(path, O_RDONLY)` then `os.lseek(fd, 0, SEEK_END)`.
    fn seek_size(&mut self, path: &str) -> Result<u64, OpenError>;

    /// `util.load_text_file(path)`; the `Err` is `str()` of the `OSError`.
    fn read_text(&mut self, path: &str) -> Result<String, String>;

    /// `util.get_mount_info(path)`.
    fn mount_info(&mut self, path: &str) -> Option<Mounted>;

    /// `util.is_container()`.
    fn is_container(&mut self) -> bool;

    /// `util.get_cmdline()`.
    fn cmdline(&mut self) -> String;

    /// `util.find_devs_with(criteria)`.
    fn find_devs_with(&mut self, criteria: &str) -> Vec<String>;

    /// `distro.get_tmp_exec_path()`.
    fn tmp_exec_path(&mut self) -> String;

    /// `temp_utils.mkdtemp(dir=dir, needs_exe=True)`.
    fn mkdtemp(&mut self, dir: &str) -> Result<String, String>;

    /// The `finally` half of `temp_utils.tempdir`: `shutil.rmtree(tdir)`.
    fn rmtree(&mut self, path: &str);

    /// `os.mkdir(path, 0o700)`.
    fn mkdir(&mut self, path: &str) -> Result<(), String>;

    /// `KEYDATA_PATH.exists()`, then its bytes.
    fn read_keydata(&mut self) -> Option<Result<String, String>>;

    /// `KEYDATA_PATH.unlink()`.
    fn unlink_keydata(&mut self) -> Result<(), String>;

    /// `distro.manage_service(action, service)`.
    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), ProcError>;
}

/// `RESIZE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
    Skipped,
    Changed,
    Nochange,
    Failed,
}

impl Resize {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Skipped => "SKIPPED",
            Self::Changed => "CHANGED",
            Self::Nochange => "NOCHANGE",
            Self::Failed => "FAILED",
        }
    }
}

/// One `(entry-in-devices, action, message)` triple.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// The config's own entry, which need not be a string.
    pub devent: Value,
    pub action: Resize,
    pub message: String,
}

/// The `RESIZERS` classes. They hold no state beyond the distro, which reaches
/// them through [`Host`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resizer {
    GrowPart,
    GrowFS,
    Gpart,
}

/// Why `resizer_factory` refused.
///
/// The distinction matters: `handle` re-raises both, but only after logging,
/// and the two are the only exceptions it catches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactoryError {
    /// `ValueError`.
    Value(String),
    /// `TypeError`.
    Type(String),
}

impl std::fmt::Display for FactoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Value(message) | Self::Type(message) => f.write_str(message),
        }
    }
}

/// Why a resize did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResizeError {
    /// `ResizeFailedException`, which `_call_resizer` turns into a `FAILED`
    /// row.
    Failed(String),
    /// An exception nothing catches, which ends the module.
    Fatal(String),
}

/// `resizer_factory`.
///
/// # Errors
/// `ValueError` when nothing is available, `TypeError` for a mode that is not
/// a resizer's name.
pub fn resizer_factory(
    host: &mut dyn Host,
    mode: &Value,
    devices: &[Value],
) -> Result<Resizer, FactoryError> {
    if mode.as_str() == Some("auto") {
        for (_, resizer) in RESIZERS {
            if resizer.available(host, devices) {
                return Ok(resizer);
            }
        }
        return Err(FactoryError::Value("No resizers available".to_owned()));
    }

    let found = mode
        .as_str()
        .and_then(|name| RESIZERS.iter().find(|(key, _)| *key == name))
        .map(|(_, resizer)| *resizer);
    let Some(resizer) = found else {
        return Err(FactoryError::Type(format!(
            "unknown resize mode {}",
            py_str(mode)
        )));
    };
    if resizer.available(host, devices) {
        return Ok(resizer);
    }
    Err(FactoryError::Value(format!(
        "mode {} not available",
        py_str(mode)
    )))
}

impl Resizer {
    /// `Resizer.available`.
    pub fn available(self, host: &mut dyn Host, devices: &[Value]) -> bool {
        match self {
            Self::GrowPart => {
                let argv = argv(&["growpart", "--help"]);
                match host.subp(&argv, &[("LANG", "C")], None) {
                    // `re.search(r"--update\s+", out)`.
                    Ok((out, _)) => contains_update_flag(&out),
                    Err(_) => false,
                }
            }
            // `os.path.isfile("/etc/rc.d/growfs") and devices == ["/"]`, in
            // that order: upstream stats before it compares.
            Self::GrowFS => {
                host.is_file("/etc/rc.d/growfs")
                    && devices.len() == 1
                    && devices.first().and_then(Value::as_str) == Some("/")
            }
            Self::Gpart => {
                let argv = argv(&["gpart", "help"]);
                // `rcs=[0, 1]`, so exit 1 is not an error and its stderr is
                // still read.
                match host.subp(&argv, &[("LANG", "C")], None) {
                    Ok((_, err)) => err.contains("gpart recover "),
                    Err(error) if error.exit_code == Some(1) => {
                        error.stderr.contains("gpart recover ")
                    }
                    Err(_) => false,
                }
            }
        }
    }

    /// `Resizer.resize`, returning the size before and after.
    fn resize(
        self,
        host: &mut dyn Host,
        diskdev: &str,
        partnum: &str,
        partdev: &str,
        fs: Option<&str>,
        log: &mut Logger,
    ) -> Result<(Option<u64>, Option<u64>), ResizeError> {
        match self {
            Self::GrowPart => {
                Self::resize_growpart(host, diskdev, partnum, partdev, fs, log)
            }
            Self::GrowFS => {
                let before = get_size(host, partdev, fs, log)?;
                host.manage_service("onestart", "growfs").map_err(|error| {
                    logexc(log, "Failed: service growfs onestart");
                    ResizeError::Failed(error.to_string())
                })?;
                Ok((before, get_size(host, partdev, fs, log)?))
            }
            Self::Gpart => {
                let recover = argv(&["gpart", "recover", diskdev]);
                if let Err(error) = host.subp(&recover, &[], None) {
                    // Upstream's guard is `!= 0`, which a raised
                    // ProcessExecutionError always satisfies.
                    if error.exit_code != Some(0) {
                        logexc(log, &format!("Failed: gpart recover {diskdev}"));
                        return Err(ResizeError::Failed(error.to_string()));
                    }
                }
                let before = get_size(host, partdev, fs, log)?;
                let resize = argv(&["gpart", "resize", "-i", partnum, diskdev]);
                if let Err(error) = host.subp(&resize, &[], None) {
                    logexc(
                        log,
                        &format!("Failed: gpart resize -i {partnum} {diskdev}"),
                    );
                    return Err(ResizeError::Failed(error.to_string()));
                }
                Ok((before, get_size(host, partdev, fs, log)?))
            }
        }
    }

    /// `ResizeGrowPart.resize`, whose temp directory exists so that
    /// `systemd-tmpfiles-clean` cannot pull growpart's state out from under it
    /// mid-run.
    fn resize_growpart(
        host: &mut dyn Host,
        diskdev: &str,
        partnum: &str,
        partdev: &str,
        fs: Option<&str>,
        log: &mut Logger,
    ) -> Result<(Option<u64>, Option<u64>), ResizeError> {
        let before = get_size(host, partdev, fs, log)?;

        let tmp_dir = host.tmp_exec_path();
        let tmpd = host.mkdtemp(&tmp_dir).map_err(ResizeError::Fatal)?;
        let growpart_tmp = join(&tmpd, "growpart");
        let env = [("LANG", "C"), ("TMPDIR", growpart_tmp.as_str())];

        // The whole body runs inside `with temp_utils.tempdir(...)`, so the
        // directory goes away on every exit from it, error included.
        let inside = (|host: &mut dyn Host, log: &mut Logger| {
            if !host.exists(&growpart_tmp) {
                host.mkdir(&growpart_tmp).map_err(ResizeError::Fatal)?;
            }
            let dry = argv(&["growpart", "--dry-run", diskdev, partnum]);
            if let Err(error) = host.subp(&dry, &env, None) {
                if error.exit_code != Some(1) {
                    logexc(
                        log,
                        &format!(
                            "Failed growpart --dry-run for ({diskdev}, {partnum})"
                        ),
                    );
                    return Err(ResizeError::Failed(error.to_string()));
                }
                return Ok(false);
            }
            let grow = argv(&["growpart", diskdev, partnum]);
            if let Err(error) = host.subp(&grow, &env, None) {
                logexc(log, &format!("Failed: growpart {diskdev} {partnum}"));
                return Err(ResizeError::Failed(error.to_string()));
            }
            Ok(true)
        })(host, log);

        host.rmtree(&tmpd);
        if !inside? {
            return Ok((before, before));
        }
        Ok((before, get_size(host, partdev, fs, log)?))
    }
}

/// `re.search(r"--update\s+", out)`.
fn contains_update_flag(out: &str) -> bool {
    out.match_indices("--update").any(|(at, _)| {
        out.get(at + "--update".len()..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
    })
}

/// `get_size`.
fn get_size(
    host: &mut dyn Host,
    filename: &str,
    fs: Option<&str>,
    log: &mut Logger,
) -> Result<Option<u64>, ResizeError> {
    match host.seek_size(filename) {
        Ok(size) => Ok(Some(size)),
        Err(OpenError::NotFound) => {
            if fs == Some("zfs") {
                return Ok(get_zfs_size(host, filename, log));
            }
            Ok(None)
        }
        Err(OpenError::Other(message)) => Err(ResizeError::Fatal(message)),
    }
}

/// `get_zfs_size`.
fn get_zfs_size(host: &mut dyn Host, dataset: &str, log: &mut Logger) -> Option<u64> {
    let zpool = dataset.split('/').next().unwrap_or(dataset);
    let argv = argv(&["zpool", "get", "-Hpovalue", "size", zpool]);
    match host.subp(&argv, &[], None) {
        Ok((size, _)) => {
            // `int(size.strip())` raises on junk, and nothing catches it.
            size.trim().parse().ok()
        }
        Err(error) => {
            log.debug(SOURCE, &format!("Failed: zpool get size {zpool}: {error}"));
            None
        }
    }
}

/// `util.rootdev_from_cmdline`, which lives in `ci-sys` because `cc_resizefs`
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

/// `Distro.get_mapped_device`.
fn get_mapped_device(
    host: &mut dyn Host,
    blockdev: &str,
    log: &mut Logger,
) -> Option<String> {
    let realpath = host.realpath(blockdev);
    if realpath.starts_with("/dev/dm-") {
        log.debug(
            "__init__.py",
            &format!("{blockdev} is a mapped device pointing to {realpath}"),
        );
        return Some(realpath);
    }
    None
}

/// `Distro.device_part_info`: an entry in `/dev` as parent disk plus partition
/// number.
fn device_part_info(
    host: &mut dyn Host,
    devpath: &str,
) -> Result<(String, String), PartError> {
    let rpath = host.realpath(devpath);
    let bname = rpath.rsplit('/').next().unwrap_or(&rpath).to_owned();
    let syspath = format!("/sys/class/block/{bname}");

    if !host.exists(&syspath) {
        return Err(PartError::Skip(format!(
            "{devpath} had no syspath ({syspath})"
        )));
    }
    let ptpath = join(&syspath, "partition");
    if !host.exists(&ptpath) {
        return Err(PartError::Skip(format!("{devpath} not a partition")));
    }
    let ptnum = host
        .read_text(&ptpath)
        .map_err(PartError::Fatal)?
        .trim_end()
        .to_owned();

    let rsyspath = host.realpath(&syspath);
    let disksyspath = dirname(&rsyspath);
    let diskmajmin = host
        .read_text(&join(&disksyspath, "dev"))
        .map_err(PartError::Fatal)?
        .trim_end()
        .to_owned();
    let diskdevpath = host.realpath(&format!("/dev/block/{diskmajmin}"));
    Ok((diskdevpath, ptnum))
}

/// `device_part_info`'s two failure shapes: the `ValueError`/`TypeError` pair
/// the caller turns into a `SKIPPED` row, and the read failure it does not
/// catch at all.
enum PartError {
    Skip(String),
    Fatal(String),
}

/// `devent2dev`'s two failure shapes.
enum DevError {
    /// `ValueError`, which the caller turns into a `SKIPPED` row.
    Value(String),
    /// Anything else, which ends the module.
    Fatal(String),
}

/// `devent2dev`.
fn devent2dev(
    host: &mut dyn Host,
    devent: &Value,
) -> Result<(String, Option<String>), DevError> {
    // `devent.startswith` on a non-string is an AttributeError, and nothing
    // between here and the stage runner catches one.
    let Some(devent) = devent.as_str() else {
        return Err(DevError::Fatal(format!(
            "'{}' object has no attribute 'startswith'",
            type_name(devent)
        )));
    };
    if devent.starts_with("/dev/") {
        return Ok((devent.to_owned(), None));
    }

    let Some(result) = host.mount_info(devent) else {
        // The `%` is inside the literal upstream, so this really is the
        // message a user sees. Bug B85.
        return Err(DevError::Value(
            "Could not determine device of '%s' % dev_ent".to_owned(),
        ));
    };
    let (dev, fs) = (result.devpth, result.fs_type);
    let container = host.is_container();

    if dev == "/dev/root" && !container {
        let cmdline = host.cmdline();
        let Some(found) = rootdev_from_cmdline(host, &cmdline) else {
            // Upstream then calls `os.path.exists(dev)` with `dev` set to
            // None, so the "Unable to find device" ValueError below it is
            // dead code and this TypeError ends the module instead. Bug B86.
            let _ = host.exists("None");
            return Err(DevError::Fatal(
                "stat: path should be string, bytes, os.PathLike or integer, \
                 not NoneType"
                    .to_owned(),
            ));
        };
        return Ok((found, Some(fs)));
    }
    Ok((dev, Some(fs)))
}

/// `is_encrypted`.
fn is_encrypted(
    host: &mut dyn Host,
    blockdev: &str,
    partition: &str,
    log: &mut Logger,
) -> bool {
    if !host.which("cryptsetup") {
        log.debug(
            SOURCE,
            "cryptsetup not found. Assuming no encrypted partitions",
        );
        return false;
    }
    let status = argv(&["cryptsetup", "status", blockdev]);
    if let Err(error) = host.subp(&status, &[], None) {
        if error.exit_code == Some(4) {
            log.debug(
                SOURCE,
                &format!("Determined that {blockdev} is not encrypted"),
            );
        } else {
            log.warning(
                SOURCE,
                &format!(
                    "Received unexpected exit code {} from cryptsetup status. \
                     Assuming no encrypted partitions.",
                    error
                        .exit_code
                        .map_or_else(|| "None".to_owned(), |code| code.to_string())
                ),
            );
        }
        return false;
    }
    let is_luks = argv(&["cryptsetup", "isLuks", partition]);
    if host.subp(&is_luks, &[], None).is_ok() {
        log.debug(SOURCE, &format!("Determined that {blockdev} is encrypted"));
        return true;
    }
    false
}

/// `get_underlying_partition`.
fn get_underlying_partition(
    host: &mut dyn Host,
    blockdev: &str,
) -> Result<String, String> {
    let command = argv(&["dmsetup", "deps", "--options=devname", blockdev]);
    let dep = host
        .subp(&command, &[], None)
        .map_err(|error| error.to_string())?
        .0;
    if !dep.starts_with("1 depend") {
        return Err(format!(
            "Expecting '1 dependencies' from 'dmsetup'. Received: {dep}"
        ));
    }
    // `dep.split(": (")[1].split(")")[0]`, whose IndexError upstream rewords.
    let Some(name) = dep
        .split_once(": (")
        .and_then(|(_, rest)| rest.split(')').next())
    else {
        return Err(format!(
            "Ran `{}`, but received unexpected stdout: `{dep}`",
            py_list(&command)
        ));
    };
    Ok(format!("/dev/{name}"))
}

/// `resize_encrypted`.
///
/// The keyfile is single-use: the slot it names is killed and the file removed
/// whether or not the resize worked, so a second run of this module finds
/// nothing to do.
fn resize_encrypted(
    host: &mut dyn Host,
    blockdev: &str,
    partition: &str,
    log: &mut Logger,
) -> Result<(Resize, String), String> {
    let Some(raw) = host.read_keydata() else {
        return Ok((Resize::Skipped, "No encryption keyfile found".to_owned()));
    };
    let loaded = raw.ok().and_then(|text| {
        let parsed = ci_core::jsonfmt::json_loads(&text)?;
        let key = parsed.get("key")?.as_str()?;
        let decoded = ci_core::b64::decode(key)?;
        let slot = py_str(parsed.get("slot")?);
        Some((decoded, slot))
    });
    let Some((decoded_key, slot)) = loaded else {
        return Err(
            "Could not load encryption key. This is expected if the volume \
             has been previously resized."
                .to_owned(),
        );
    };

    let resize = argv(&["cryptsetup", "--key-file", "-", "resize", blockdev]);
    let outcome = host.subp(&resize, &[], Some(&decoded_key));

    // The `finally` runs before the resize failure is re-raised.
    let kill = argv(&[
        "cryptsetup",
        "luksKillSlot",
        "--batch-mode",
        partition,
        &slot,
    ]);
    if let Err(error) = host.subp(&kill, &[], None) {
        log.warning(
            SOURCE,
            &format!(
                "Failed to kill luks slot after resizing encrypted volume: {error}"
            ),
        );
    }
    if host.unlink_keydata().is_err() {
        logexc(
            log,
            "Failed to remove keyfile after resizing encrypted volume",
        );
    }
    outcome.map_err(|error| error.to_string())?;

    Ok((
        Resize::Changed,
        format!("Successfully resized encrypted volume '{blockdev}'"),
    ))
}

/// What the mapped-device branch decided.
enum Mapped {
    Done(Resize, String),
    /// Nothing yet: the named partition must be resized first.
    Requeue(String),
}

/// The `underlying_blockdev` branch of `resize_devices`, whose every exception
/// the caller turns into one `FAILED` row.
fn resize_mapped(
    host: &mut dyn Host,
    blockdev: &str,
    underlying: &str,
    info: &[Outcome],
    log: &mut Logger,
) -> Result<Mapped, String> {
    let partition = get_underlying_partition(host, blockdev)?;
    if !is_encrypted(host, underlying, &partition, log) {
        return Ok(Mapped::Done(
            Resize::Skipped,
            format!(
                "Resizing mapped device ({blockdev}) skipped as it is not encrypted."
            ),
        ));
    }
    if !info
        .iter()
        .any(|seen| seen.devent.as_str() == Some(partition.as_str()))
    {
        return Ok(Mapped::Requeue(partition));
    }
    let (action, message) = resize_encrypted(host, blockdev, &partition, log)?;
    Ok(Mapped::Done(action, message))
}

/// `_call_resizer`.
#[allow(clippy::too_many_arguments)]
fn call_resizer(
    host: &mut dyn Host,
    resizer: Resizer,
    devent: &Value,
    disk: Option<&str>,
    ptnum: Option<&str>,
    blockdev: &str,
    fs: Option<&str>,
    log: &mut Logger,
) -> Result<Vec<Outcome>, String> {
    let sizes = resizer.resize(
        host,
        disk.unwrap_or("None"),
        ptnum.unwrap_or("None"),
        blockdev,
        fs,
        log,
    );
    let (old, new) = match sizes {
        Ok(sizes) => sizes,
        Err(ResizeError::Failed(message)) => {
            return Ok(vec![Outcome {
                devent: devent.clone(),
                action: Resize::Failed,
                message: format!(
                    "failed to resize: disk={}, ptnum={}: {message}",
                    disk.unwrap_or("None"),
                    ptnum.unwrap_or("None"),
                ),
            }]);
        }
        Err(ResizeError::Fatal(message)) => return Err(message),
    };

    // `disk is not None and ptnum is None` never holds for the growpart path,
    // where the two arrive together; it is the zfs/growfs path, where both are
    // None, that decides which message shape is used.
    let named = disk.is_some() && ptnum.is_none();
    let message = if old == new {
        format!(
            "no change necessary ({}, {})",
            disk.unwrap_or("None"),
            ptnum.unwrap_or("None")
        )
    } else if old.is_none() || new.is_none() {
        if named {
            format!(
                "changed ({}, {}) size, new size is unknown",
                disk.unwrap_or("None"),
                ptnum.unwrap_or("None")
            )
        } else {
            format!("changed ({blockdev}) size, new size is unknown")
        }
    } else if named {
        format!(
            "changed ({}, {}) from {} to {}",
            disk.unwrap_or("None"),
            ptnum.unwrap_or("None"),
            show_size(old),
            show_size(new)
        )
    } else {
        format!(
            "changed ({blockdev}) from {} to {}",
            show_size(old),
            show_size(new)
        )
    };
    let action = if old == new {
        Resize::Nochange
    } else {
        Resize::Changed
    };
    Ok(vec![Outcome {
        devent: devent.clone(),
        action,
        message,
    }])
}

fn show_size(size: Option<u64>) -> String {
    size.map_or_else(|| "None".to_owned(), |value| value.to_string())
}

/// `resize_devices`.
///
/// # Errors
/// An exception upstream lets escape, which ends the module with nothing
/// recorded for the devices after it.
pub fn resize_devices(
    host: &mut dyn Host,
    resizer: Resizer,
    devices: &[Value],
    log: &mut Logger,
) -> Result<Vec<Outcome>, String> {
    let mut devices: std::collections::VecDeque<Value> =
        devices.iter().cloned().collect();
    let mut info: Vec<Outcome> = Vec::new();

    while let Some(devent) = devices.pop_front() {
        let (blockdev, fs) = match devent2dev(host, &devent) {
            Ok(found) => found,
            Err(DevError::Value(message)) => {
                info.push(Outcome {
                    devent,
                    action: Resize::Skipped,
                    message: format!("unable to convert to device: {message}"),
                });
                continue;
            }
            Err(DevError::Fatal(message)) => return Err(message),
        };
        let fs = fs.as_deref();

        log.debug(
            SOURCE,
            &format!("growpart found fs={}", fs.unwrap_or("None")),
        );

        if fs == Some("zfs") && resizer == Resizer::GrowFS {
            info.extend(call_resizer(
                host, resizer, &devent, None, None, &blockdev, fs, log,
            )?);
            continue;
        }

        let mode = match host.stat_mode(&blockdev) {
            Ok(mode) => mode,
            Err(message) => {
                info.push(Outcome {
                    devent,
                    action: Resize::Skipped,
                    message: format!("stat of '{blockdev}' failed: {message}"),
                });
                continue;
            }
        };
        // `stat.S_ISBLK` and `stat.S_ISCHR`.
        let kind = mode & 0o170_000;
        if kind != 0o60_000 && kind != 0o20_000 {
            info.push(Outcome {
                devent,
                action: Resize::Skipped,
                message: format!("device '{blockdev}' not a block device"),
            });
            continue;
        }

        if let Some(underlying) = get_mapped_device(host, &blockdev, log) {
            match resize_mapped(host, &blockdev, &underlying, &info, log) {
                Ok(Mapped::Done(action, message)) => info.push(Outcome {
                    devent,
                    action,
                    message,
                }),
                // The underlying partition has to grow first, so both go back
                // on the queue with it in front.
                Ok(Mapped::Requeue(partition)) => {
                    devices.push_front(devent);
                    devices.push_front(Value::from(partition));
                }
                Err(message) => info.push(Outcome {
                    devent,
                    action: Resize::Failed,
                    message: format!(
                        "Resizing encrypted device ({blockdev}) failed: {message}"
                    ),
                }),
            }
            // A non-encrypted mapped device is never resized, encrypted or
            // not; upstream stops here either way.
            continue;
        }

        let (disk, ptnum) = match device_part_info(host, &blockdev) {
            Ok(found) => found,
            Err(PartError::Skip(message)) => {
                info.push(Outcome {
                    devent,
                    action: Resize::Skipped,
                    message: format!("device_part_info({blockdev}) failed: {message}"),
                });
                continue;
            }
            Err(PartError::Fatal(message)) => return Err(message),
        };

        info.extend(call_resizer(
            host,
            resizer,
            &devent,
            Some(&disk),
            Some(&ptnum),
            &blockdev,
            fs,
            log,
        )?);
    }

    Ok(info)
}

/// `util.logexc`: the message at warning, then again at debug where upstream
/// attaches the traceback this port does not have. Both are attributed to
/// `log_util.py`, which is the frame `logging` sees.
pub(crate) fn logexc(log: &mut Logger, message: &str) {
    log.warning("log_util.py", message);
    log.debug("log_util.py", message);
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// `os.path.join` for the shapes this module builds.
fn join(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        return format!("{directory}{name}");
    }
    format!("{directory}/{name}")
}

/// `os.path.dirname`.
fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(at) => path.get(..at).unwrap_or_default().to_owned(),
        None => String::new(),
    }
}

/// `util.get_cfg_option_list(cfg, key, default)`.
fn cfg_option_list(map: &Object, key: &str, default: &[Value]) -> Vec<Value> {
    match map.get(key) {
        None => default.to_vec(),
        Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![Value::from(py_str(other))],
    }
}

/// `lifecycle.deprecate(deprecated_version="22.2", ..)`.
fn deprecate(log: &mut Logger, deprecated: &str, extra: &str) {
    log.log(
        ci_log::Level::Deprecated,
        "lifecycle.py",
        &format!(
            "{deprecated} is deprecated in 22.2 and scheduled to be removed \
             in 27.2. {extra}"
        ),
    );
}

/// The decisions `handle` takes before it touches a device, so that the
/// differential can drive them without one.
///
/// # Errors
/// The `ValueError`/`TypeError` upstream re-raises when an explicit `mode`
/// names a resizer that is not there.
pub fn plan(
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<Option<(Resizer, Vec<Value>)>, String> {
    let default = default_config();
    let mycfg = if let Some(value) = cfg.get("growpart") {
        value.clone()
    } else {
        log.debug(
            SOURCE,
            &format!(
                "No 'growpart' entry in cfg.  Using default: {}",
                ci_config::repr(&Value::Object(default.clone()))
            ),
        );
        Value::Object(default)
    };
    let Some(mycfg) = mycfg.as_object() else {
        log.warning(SOURCE, "'growpart' in config was not a dict");
        return Ok(None);
    };

    let auto = Value::from("auto");
    let mode = mycfg.get("mode").unwrap_or(&auto);
    if ci_config::option::is_false(mode) {
        if mode.as_str() != Some("off") {
            deprecate(
                log,
                &format!("Growpart's 'mode' key with value '{}'", py_str(mode)),
                "Use 'off' instead.",
            );
        }
        log.debug(SOURCE, &format!("growpart disabled: mode={}", py_str(mode)));
        return Ok(None);
    }

    let ignore = mycfg
        .get("ignore_growroot_disabled")
        .cloned()
        .unwrap_or(Value::Bool(false));
    if ci_config::option::is_false(&ignore) && host.is_file("/etc/growroot-disabled") {
        log.debug(SOURCE, "growpart disabled: /etc/growroot-disabled exists");
        log.debug(SOURCE, "use ignore_growroot_disabled to ignore");
        return Ok(None);
    }

    let devices = cfg_option_list(mycfg, "devices", &[Value::from("/")]);
    if devices.is_empty() {
        log.debug(SOURCE, "growpart: empty device list");
        return Ok(None);
    }

    match resizer_factory(host, mode, &devices) {
        Ok(resizer) => Ok(Some((resizer, devices))),
        Err(error) => {
            log.debug(
                SOURCE,
                &format!(
                    "growpart unable to find resizer for '{}': {error}",
                    py_str(mode)
                ),
            );
            if mode.as_str() == Some("auto") {
                return Ok(None);
            }
            Err(error.to_string())
        }
    }
}

/// `handle`, given a machine to run against.
///
/// # Errors
/// Whatever upstream lets escape `handle`.
pub fn handle_with(
    cfg: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) -> Result<Vec<Outcome>, String> {
    let Some((resizer, devices)) = plan(cfg, host, log)? else {
        return Ok(Vec::new());
    };
    let outcomes = resize_devices(host, resizer, &devices, log)?;
    for outcome in &outcomes {
        let entry = py_str(&outcome.devent);
        if outcome.action == Resize::Changed {
            log.info(SOURCE, &format!("'{entry}' resized: {}", outcome.message));
        } else {
            log.debug(
                SOURCE,
                &format!("'{entry}' {}: {}", outcome.action.name(), outcome.message),
            );
        }
    }
    Ok(outcomes)
}

/// `handle`.
///
/// # Errors
/// Whatever upstream lets escape `handle`.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    // Nothing below is rootable: `growpart` and `cryptsetup` act on the
    // machine's own device nodes. A rooted run is a test, and a test must not
    // repartition the host.
    if args.root != Path::new("/") {
        return Ok(());
    }
    let mut host = Live::new(args.distro);
    let cfg = args.cfg.clone();
    handle_with(&cfg, &mut host, &mut *args.logger).map(|_| ())
}

/// The real machine.
#[derive(Debug)]
pub struct Live<'a> {
    distro: &'a ci_distro::Distro,
}

impl<'a> Live<'a> {
    #[must_use]
    pub const fn new(distro: &'a ci_distro::Distro) -> Self {
        Self { distro }
    }
}

impl Host for Live<'_> {
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(&str, &str)],
        data: Option<&[u8]>,
    ) -> Result<(String, String), ProcError> {
        let mut command = ci_sys::subp::Subp::new(argv);
        for (key, value) in env {
            command = command.env(key, value);
        }
        if let Some(data) = data {
            command = command.stdin(data.to_vec());
        }
        let out = command.run().map_err(|error| ProcError {
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

    fn which(&mut self, program: &str) -> bool {
        ci_sys::subp::which(program).is_some()
    }

    fn exists(&mut self, path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn is_file(&mut self, path: &str) -> bool {
        std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
    }

    fn realpath(&mut self, path: &str) -> String {
        std::fs::canonicalize(path).map_or_else(
            |_| path.to_owned(),
            |resolved| resolved.to_string_lossy().into_owned(),
        )
    }

    fn stat_mode(&mut self, path: &str) -> Result<u32, String> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .map(|meta| meta.mode())
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(path)))
    }

    fn seek_size(&mut self, path: &str) -> Result<u64, OpenError> {
        use std::io::Seek;
        let mut file = std::fs::File::open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                OpenError::NotFound
            } else {
                OpenError::Other(ci_core::pyerr::oserror(&error, Path::new(path)))
            }
        })?;
        file.seek(std::io::SeekFrom::End(0)).map_err(|error| {
            OpenError::Other(ci_core::pyerr::oserror(&error, Path::new(path)))
        })
    }

    fn read_text(&mut self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path)
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(path)))
    }

    fn mount_info(&mut self, path: &str) -> Option<Mounted> {
        ci_sys::mount::get_mount_info(path).map(|info| Mounted {
            devpth: info.devpth,
            fs_type: info.fs_type,
            mount_point: info.mount_point,
        })
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

    fn tmp_exec_path(&mut self) -> String {
        // `temp_utils.get_tmp_ancestor(needs_exe=True)`.
        let ancestor = "/var/tmp/cloud-init";
        if ci_sys::mount::has_mount_opt(ancestor, "noexec") {
            return join(&join(self.distro.usr_lib_exec, "cloud-init"), "clouddir");
        }
        ancestor.to_owned()
    }

    fn mkdtemp(&mut self, dir: &str) -> Result<String, String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(dir)))?;
        for attempt in 0..64_u32 {
            let candidate = format!("{dir}/tmp{}{attempt}", std::process::id());
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(ci_core::pyerr::oserror(&error, Path::new(&candidate)))
                }
            }
        }
        Err(format!("[Errno 17] File exists: '{dir}'"))
    }

    fn rmtree(&mut self, path: &str) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir(path)
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(path)))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(path)))
    }

    fn read_keydata(&mut self) -> Option<Result<String, String>> {
        if !Path::new(KEYDATA_PATH).exists() {
            return None;
        }
        Some(
            std::fs::read_to_string(KEYDATA_PATH).map_err(|error| {
                ci_core::pyerr::oserror(&error, Path::new(KEYDATA_PATH))
            }),
        )
    }

    fn unlink_keydata(&mut self) -> Result<(), String> {
        std::fs::remove_file(KEYDATA_PATH)
            .map_err(|error| ci_core::pyerr::oserror(&error, Path::new(KEYDATA_PATH)))
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), ProcError> {
        // `onestart` is an rc.d action; no Linux init system in `ci-distro`
        // has one, and upstream's own table would raise a KeyError here.
        Err(ProcError {
            argv: vec![action.to_owned(), service.to_owned()],
            exit_code: None,
            stdout: String::new(),
            stderr: format!("unsupported service action '{action}'"),
        })
    }
}

/// What a scripted command did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A [`Host`] whose every answer is set up front.
///
/// This is what `dump-cc-growpart` drives, so the differential can compare the
/// order the module probes a machine in and not only what it concluded. A
/// question with no scripted answer gets the same "no" on both sides rather
/// than a panic, because the interesting cases are the ones where the two
/// implementations ask *different* questions.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Keyed by the argv joined with spaces.
    pub commands: Vec<(String, CommandResult)>,
    pub which: Vec<String>,
    pub exists: Vec<String>,
    pub files: Vec<String>,
    pub realpath: Vec<(String, String)>,
    /// `st_mode`, which the fixture gives in octal.
    pub stat: Vec<(String, u32)>,
    /// One entry per read, so that a device can be bigger the second time
    /// `get_size` asks. `None` is a device that is not there for that read,
    /// and the last value repeats.
    pub sizes: Vec<(String, Vec<Option<u64>>)>,
    pub text: Vec<(String, String)>,
    pub mounts: Vec<(String, Mounted)>,
    pub container: bool,
    pub cmdline: String,
    pub devs: Vec<(String, Vec<String>)>,
    pub tmp_exec: String,
    pub tmpdir: String,
    pub keydata: Option<String>,
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
    fn subp(
        &mut self,
        argv: &[String],
        env: &[(&str, &str)],
        data: Option<&[u8]>,
    ) -> Result<(String, String), ProcError> {
        let key = argv.join(" ");
        let shown: Vec<String> = env
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        self.record(format!(
            "subp {key} env={} data={}",
            shown.join(","),
            data.map_or(-1, |bytes| i64::try_from(bytes.len()).unwrap_or(-1))
        ));
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

    fn which(&mut self, program: &str) -> bool {
        self.record(format!("which {program}"));
        self.which.iter().any(|name| name == program)
    }

    fn exists(&mut self, path: &str) -> bool {
        self.record(format!("exists {path}"));
        self.exists.iter().any(|name| name == path)
    }

    fn is_file(&mut self, path: &str) -> bool {
        self.record(format!("isfile {path}"));
        self.files.iter().any(|name| name == path)
    }

    fn realpath(&mut self, path: &str) -> String {
        self.record(format!("realpath {path}"));
        Self::lookup(&self.realpath, path).map_or_else(|| path.to_owned(), Clone::clone)
    }

    fn stat_mode(&mut self, path: &str) -> Result<u32, String> {
        self.record(format!("stat {path}"));
        Self::lookup(&self.stat, path)
            .copied()
            .ok_or_else(|| format!("[Errno 2] No such file or directory: '{path}'"))
    }

    fn seek_size(&mut self, path: &str) -> Result<u64, OpenError> {
        self.record(format!("size {path}"));
        let Some((_, values)) = self.sizes.iter_mut().find(|(name, _)| name == path)
        else {
            return Err(OpenError::NotFound);
        };
        let first = *values.first().ok_or(OpenError::NotFound)?;
        if values.len() > 1 {
            values.remove(0);
        }
        first.ok_or(OpenError::NotFound)
    }

    fn read_text(&mut self, path: &str) -> Result<String, String> {
        self.record(format!("read {path}"));
        Self::lookup(&self.text, path)
            .cloned()
            .ok_or_else(|| format!("[Errno 2] No such file or directory: '{path}'"))
    }

    fn mount_info(&mut self, path: &str) -> Option<Mounted> {
        self.record(format!("mount_info {path}"));
        Self::lookup(&self.mounts, path).cloned()
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

    fn tmp_exec_path(&mut self) -> String {
        self.record("tmp_exec_path".to_owned());
        self.tmp_exec.clone()
    }

    fn mkdtemp(&mut self, dir: &str) -> Result<String, String> {
        self.record(format!("mkdtemp {dir}"));
        Ok(self.tmpdir.clone())
    }

    fn rmtree(&mut self, path: &str) {
        self.record(format!("rmtree {path}"));
    }

    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        self.record(format!("mkdir {path}"));
        Ok(())
    }

    fn read_keydata(&mut self) -> Option<Result<String, String>> {
        self.record("read_keydata".to_owned());
        self.keydata.clone().map(Ok)
    }

    fn unlink_keydata(&mut self) -> Result<(), String> {
        self.record("unlink_keydata".to_owned());
        self.keydata = None;
        Ok(())
    }

    fn manage_service(&mut self, action: &str, service: &str) -> Result<(), ProcError> {
        self.record(format!("manage_service {action} {service}"));
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn commands(items: &[(&str, &str)]) -> Vec<(String, CommandResult)> {
        items
            .iter()
            .map(|(key, stdout)| {
                (
                    (*key).to_owned(),
                    CommandResult {
                        exit_code: 0,
                        stdout: (*stdout).to_owned(),
                        stderr: String::new(),
                    },
                )
            })
            .collect()
    }

    fn mounted(dev: &str, fs: &str) -> Mounted {
        Mounted {
            devpth: dev.to_owned(),
            fs_type: fs.to_owned(),
            mount_point: "/".to_owned(),
        }
    }

    /// The machine the case files use: growpart installed, root on
    /// `/dev/sda1`, a second partition, and the `/sys` entries behind both.
    fn machine() -> Fixture {
        Fixture {
            commands: commands(&[
                ("growpart --help", "growpart\n  --update  the kernel\n"),
                ("growpart --dry-run /dev/sda 2", "CHANGE: partition=2\n"),
                ("growpart /dev/sda 2", "CHANGED: partition=2\n"),
            ]),
            realpath: pairs(&[
                ("/sys/class/block/sda2", "/sys/devices/block/sda/sda2"),
                ("/dev/block/8:0", "/dev/sda"),
            ]),
            stat: vec![
                ("/dev/sda2".to_owned(), 0o60660),
                ("/dev/dm-0".to_owned(), 0o60660),
            ],
            exists: owned(&[
                "/sys/class/block/sda2",
                "/sys/class/block/sda2/partition",
            ]),
            text: pairs(&[
                ("/sys/class/block/sda2/partition", "2\n"),
                ("/sys/devices/block/sda/dev", "8:0\n"),
            ]),
            sizes: vec![("/dev/sda2".to_owned(), vec![Some(1024), Some(2048)])],
            mounts: vec![("/".to_owned(), mounted("/dev/sda2", "ext4"))],
            tmp_exec: "/var/tmp/cloud-init".to_owned(),
            tmpdir: "/var/tmp/cloud-init/tmp0".to_owned(),
            ..Fixture::default()
        }
    }

    /// A machine whose root is a mapped LUKS volume over `/dev/sda2`.
    fn encrypted() -> Fixture {
        let mut host = machine();
        host.commands.extend(commands(&[
            (
                "dmsetup deps --options=devname /dev/dm-0",
                "1 dependencies\t: (sda2)\n",
            ),
            ("cryptsetup status /dev/dm-0", ""),
            ("cryptsetup isLuks /dev/sda2", ""),
            ("cryptsetup --key-file - resize /dev/dm-0", ""),
            ("cryptsetup luksKillSlot --batch-mode /dev/sda2 5", ""),
        ]));
        host.which = owned(&["cryptsetup"]);
        host.mounts = vec![("/".to_owned(), mounted("/dev/dm-0", "ext4"))];
        host.keydata = Some(r#"{"key": "MTIzNA==", "slot": 5}"#.to_owned());
        host
    }

    /// A machine that only has FreeBSD's `growfs`, on zfs.
    fn growfs() -> Fixture {
        Fixture {
            files: owned(&["/etc/rc.d/growfs"]),
            commands: commands(&[("zpool get -Hpovalue size tank", "4096\n")]),
            mounts: vec![("/".to_owned(), mounted("tank/root", "zfs"))],
            ..Fixture::default()
        }
    }

    fn run(host: &mut dyn Host) -> (Result<Vec<Outcome>, String>, Vec<String>) {
        let mut log = ci_log::Logger::capturing();
        let outcome = handle_with(&Object::new(), host, &mut log);
        (outcome, log.captured().to_vec())
    }

    /// A [`Fixture`] with one answer replaced, for the failures a case file
    /// cannot spell: the machine says no to something the module only does on
    /// the way to a resize.
    struct Failing {
        inner: Fixture,
        at: Break,
    }

    #[derive(PartialEq, Eq)]
    enum Break {
        Mkdtemp,
        Mkdir,
        Service,
        Unlink,
    }

    impl Failing {
        fn of(inner: Fixture, at: Break) -> Self {
            Self { inner, at }
        }
    }

    impl Host for Failing {
        fn subp(
            &mut self,
            argv: &[String],
            env: &[(&str, &str)],
            data: Option<&[u8]>,
        ) -> Result<(String, String), ProcError> {
            self.inner.subp(argv, env, data)
        }
        fn which(&mut self, program: &str) -> bool {
            self.inner.which(program)
        }
        fn exists(&mut self, path: &str) -> bool {
            self.inner.exists(path)
        }
        fn is_file(&mut self, path: &str) -> bool {
            self.inner.is_file(path)
        }
        fn realpath(&mut self, path: &str) -> String {
            self.inner.realpath(path)
        }
        fn stat_mode(&mut self, path: &str) -> Result<u32, String> {
            self.inner.stat_mode(path)
        }
        fn seek_size(&mut self, path: &str) -> Result<u64, OpenError> {
            self.inner.seek_size(path)
        }
        fn read_text(&mut self, path: &str) -> Result<String, String> {
            self.inner.read_text(path)
        }
        fn mount_info(&mut self, path: &str) -> Option<Mounted> {
            self.inner.mount_info(path)
        }
        fn is_container(&mut self) -> bool {
            self.inner.is_container()
        }
        fn cmdline(&mut self) -> String {
            self.inner.cmdline()
        }
        fn find_devs_with(&mut self, criteria: &str) -> Vec<String> {
            self.inner.find_devs_with(criteria)
        }
        fn tmp_exec_path(&mut self) -> String {
            self.inner.tmp_exec_path()
        }
        fn mkdtemp(&mut self, dir: &str) -> Result<String, String> {
            if self.at == Break::Mkdtemp {
                return Err(format!("[Errno 13] Permission denied: '{dir}'"));
            }
            self.inner.mkdtemp(dir)
        }
        fn rmtree(&mut self, path: &str) {
            self.inner.rmtree(path);
        }
        fn mkdir(&mut self, path: &str) -> Result<(), String> {
            if self.at == Break::Mkdir {
                return Err(format!("[Errno 13] Permission denied: '{path}'"));
            }
            self.inner.mkdir(path)
        }
        fn read_keydata(&mut self) -> Option<Result<String, String>> {
            self.inner.read_keydata()
        }
        fn unlink_keydata(&mut self) -> Result<(), String> {
            if self.at == Break::Unlink {
                return Err("[Errno 13] Permission denied".to_owned());
            }
            self.inner.unlink_keydata()
        }
        fn manage_service(
            &mut self,
            action: &str,
            service: &str,
        ) -> Result<(), ProcError> {
            if self.at == Break::Service {
                return Err(ProcError {
                    argv: vec![action.to_owned(), service.to_owned()],
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: format!("{service}: not found"),
                });
            }
            self.inner.manage_service(action, service)
        }
    }

    #[test]
    fn update_flag_needs_whitespace_after_it() {
        assert!(contains_update_flag("  -u | --update  update the kernel\n"));
        assert!(contains_update_flag("--update\n"));
        assert!(contains_update_flag("--update\t"));
        assert!(!contains_update_flag("--update"));
        assert!(!contains_update_flag("--updated table\n"));
        assert!(!contains_update_flag("usage: growpart\n"));
    }

    #[test]
    fn process_error_indents_every_line_of_output() {
        let error = ProcError {
            argv: owned(&["growpart", "/dev/sda", "1"]),
            exit_code: Some(2),
            stdout: "one\ntwo\n".to_owned(),
            stderr: String::new(),
        };
        assert_eq!(
            error.to_string(),
            "Unexpected error while running command.\n\
             Command: ['growpart', '/dev/sda', '1']\n\
             Exit code: 2\n\
             Reason: -\n\
             Stdout: one\n        two\n\
             Stderr: "
        );
    }

    #[test]
    fn a_grown_partition_reports_both_sizes() {
        let (outcome, log) = run(&mut machine());
        let outcome = outcome.unwrap();
        assert_eq!(outcome.len(), 1);
        assert_eq!(outcome[0].action, Resize::Changed);
        assert_eq!(outcome[0].message, "changed (/dev/sda2) from 1024 to 2048");
        assert!(log.iter().any(|line| line.contains("'/' resized: changed")));
    }

    #[test]
    fn a_temp_directory_that_cannot_be_made_ends_the_module() {
        let mut host = Failing::of(machine(), Break::Mkdtemp);
        let (outcome, _) = run(&mut host);
        assert_eq!(
            outcome,
            Err("[Errno 13] Permission denied: '/var/tmp/cloud-init'".to_owned())
        );
    }

    #[test]
    fn growparts_own_temp_directory_failing_ends_the_module() {
        let mut host = Failing::of(machine(), Break::Mkdir);
        let (outcome, _) = run(&mut host);
        assert_eq!(
            outcome,
            Err(
                "[Errno 13] Permission denied: '/var/tmp/cloud-init/tmp0/growpart'"
                    .to_owned()
            )
        );
        // The directory still goes away: it is a `with` block upstream.
        assert!(host
            .inner
            .calls
            .iter()
            .any(|call| call == "rmtree /var/tmp/cloud-init/tmp0"));
    }

    #[test]
    fn a_growfs_service_that_will_not_start_is_a_failed_row() {
        let mut host = Failing::of(growfs(), Break::Service);
        let (outcome, _) = run(&mut host);
        let outcome = outcome.unwrap();
        assert_eq!(outcome[0].action, Resize::Failed);
        assert!(
            outcome[0]
                .message
                .contains("Command: ['onestart', 'growfs']"),
            "{}",
            outcome[0].message
        );
    }

    #[test]
    fn growfs_only_ever_resizes_the_root() {
        let mut cfg = Object::new();
        cfg.insert(
            "growpart".to_owned(),
            serde_json::json!({"devices": ["/", "/srv"]}),
        );
        let mut host = growfs();
        let mut log = ci_log::Logger::capturing();
        assert!(handle_with(&cfg, &mut host, &mut log).unwrap().is_empty());
        assert!(log
            .captured()
            .iter()
            .any(|line| line.contains("unable to find resizer")));
    }

    #[test]
    fn a_keyfile_that_cannot_be_removed_only_warns() {
        let mut host = Failing::of(encrypted(), Break::Unlink);
        let (outcome, log) = run(&mut host);
        let outcome = outcome.unwrap();
        // The partition first, then the volume over it.
        assert_eq!(outcome[0].devent, Value::from("/dev/sda2"));
        assert_eq!(outcome[1].action, Resize::Changed);
        assert_eq!(
            outcome[1].message,
            "Successfully resized encrypted volume '/dev/dm-0'"
        );
        assert!(log.iter().any(|line| line
            == "log_util.py[WARNING]: Failed to remove keyfile after resizing \
                encrypted volume"));
    }

    #[test]
    fn a_used_up_keyfile_leaves_nothing_for_the_next_run() {
        let mut host = encrypted();
        let (outcome, _) = run(&mut host);
        assert_eq!(outcome.unwrap()[1].action, Resize::Changed);
        assert_eq!(host.keydata, None);
        let (outcome, _) = run(&mut host);
        assert_eq!(outcome.unwrap()[1].action, Resize::Skipped);
    }
}
