//! Port of `cloudinit/config/cc_mounts.py`.
//!
//! The module rewrites `/etc/fstab` from tenant config, so a mistake here is
//! not a failed boot but a machine that will not come back after the next one.
//! Upstream guards against that in one direction only: it refuses to name a
//! device it cannot see, and it tags every line it owns with
//! `comment=cloudconfig` so the next run can tell its own entries from the
//! image's. It does not guard the shape of the config at all, and several
//! plausible-looking shapes end the module with a bare `IndexError`.
//!
//! This file holds the decision half — everything upstream works out before it
//! touches the disk. The probes it needs (does this partition exist, is this a
//! block device, what is already in fstab) are real syscalls under a `root`,
//! the same arrangement `ci_ssh::install` uses, so a fixture tree can put the
//! code in situations this machine is not in.

use std::path::Path;

use ci_config::{Object, Value};

use super::Args;

const SOURCE: &str = "cc_mounts.py";

/// Matches `sda`, `sda1`, `xvda`, `hda`, `vdd1`, `sr0`.
///
/// Transcribed with `\n?$` because Python's `$` also matches immediately
/// before a trailing newline, so upstream accepts `"sda1\n"` — and the names
/// come from tenant config, which can contain one.
const DEVICE_NAME_FILTER: &str = r"^([x]{0,1}[shv]d[a-z][0-9]*|sr[0-9]+)\n?$";

/// Matches `server:/path`. `.` excludes newlines in both languages, so this
/// one transcribes directly.
const NETWORK_NAME_FILTER: &str = r"^.+:.*";

const FSTAB_PATH: &str = "/etc/fstab";
const MNT_COMMENT: &str = "comment=cloudconfig";
const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;

/// One thing the module does, in order. The log lines are steps for the same
/// reason as in `cc_snap`: upstream interleaves them with the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Debug(String),
    Warning(String),
    Info(String),
    /// `util.ensure_dir` inside `create_swapfile`, where a failure raises.
    EnsureDir(String),
    /// `util.ensure_dir` over the mount points, where a failure is only
    /// logged and the module carries on.
    EnsureConfigDir(String),
    /// `truncate -s 0` then `chattr +C`, which is how a swap file is made
    /// no-COW before btrfs ever sees a write to it.
    BtrfsPrepare(String),
    /// `fallocate -l <mib>M` or `dd if=/dev/zero bs=1M count=<mib>`.
    CreateSwap {
        path: String,
        mib: String,
        method: SwapMethod,
        /// Only ever logged; the method it would have selected is already
        /// decided by the time the plan is built.
        fstype: String,
    },
    /// `util.chmod(fname, 0o600)`, guarded by the file existing.
    ChmodSwap(String),
    Mkswap(String),
    /// `util.write_file(FSTAB_PATH, contents)`.
    WriteFstab(String),
    /// `swapon -a`.
    SwapOn,
    /// `mount -a`, then `systemctl daemon-reload` under systemd. Skipped
    /// entirely when nothing changed and every directory is already mounted,
    /// which only the run half can tell.
    MountAll {
        daemon_reload: bool,
        changes_made: bool,
        dirs: Vec<String>,
    },
}

/// How `create_swapfile` will lay the file down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMethod {
    /// `fallocate`, falling back to `dd` if it fails at run time.
    Fallocate,
    /// `dd` only, which is what xfs below kernel 4.18 needs.
    Dd,
}

impl SwapMethod {
    /// The name upstream interpolates into its log messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Fallocate => "fallocate",
            Self::Dd => "dd",
        }
    }
}

/// `cloud.device_name_to_device`, the datasource hook that turns a metadata
/// name such as `ephemeral0` into a device.
///
/// Base `DataSource` answers `None`; EC2, Azure, `OpenStack` and `SmartOS`
/// override it. None of those overrides is ported yet, so the live caller
/// passes a closure that always answers `None` — see COMPAT.md.
pub type Transformer<'a> = &'a dyn Fn(&str) -> Option<String>;

/// `util.expand_dotted_devname`.
///
/// Lives here rather than beside the other `util` helpers because its two
/// callers are both config modules: this one and `cc_disk_setup`, which reads
/// it from here.
#[must_use]
pub fn expand_dotted_devname(dotted: &str) -> (&str, Option<&str>) {
    match dotted.rsplit_once('.') {
        Some((device, partition)) => (device, Some(partition)),
        None => (dotted, None),
    }
}

/// `is_meta_device_name`.
#[must_use]
pub fn is_meta_device_name(name: &str) -> bool {
    if matches!(name, "ami" | "root" | "swap") {
        return true;
    }
    ["ephemeral", "ebs"]
        .iter()
        .any(|prefix| name.starts_with(prefix) && !name.contains(':'))
}

/// `is_network_device`.
#[must_use]
pub fn is_network_device(name: &str) -> bool {
    matches(NETWORK_NAME_FILTER, name)
}

fn matches(pattern: &str, text: &str) -> bool {
    regex::Regex::new(pattern).is_ok_and(|re| re.is_match(text))
}

/// `_get_nth_partition_for_device`.
fn nth_partition_for_device(
    root: &Path,
    device_path: &str,
    partition: &str,
) -> Option<String> {
    [
        format!("{device_path}{partition}"),
        format!("{device_path}p{partition}"),
        format!("{device_path}-part{partition}"),
    ]
    .into_iter()
    .find(|candidate| super::rooted(root, candidate).exists())
}

/// `_is_block_device`.
///
/// The test is whether the name appears under `/sys/block`, which is what
/// separates a real disk from a file someone dropped in `/dev`.
fn is_block_device(
    root: &Path,
    device_path: &str,
    partition_path: Option<&str>,
) -> bool {
    let mut sys_path = Path::new("/sys/block").join(real_basename(root, device_path));
    if let Some(partition_path) = partition_path {
        sys_path = sys_path.join(real_basename(root, partition_path));
    }
    super::rooted(root, &sys_path.to_string_lossy()).exists()
}

/// `os.path.realpath(p).split("/")[-1]`. `realpath` does not fail on a missing
/// path, so an unresolvable one keeps its own last component.
fn real_basename(root: &Path, path: &str) -> String {
    let rooted = super::rooted(root, path);
    let resolved = std::fs::canonicalize(&rooted).unwrap_or(rooted);
    resolved
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// `sanitize_devname`.
///
/// Answers `None` for "there is no such device", which the caller turns into
/// dropping the entry rather than writing a line naming a device that is not
/// there.
pub fn sanitize_devname(
    root: &Path,
    startname: &str,
    transformer: Transformer<'_>,
    aliases: &Object,
    steps: &mut Vec<Step>,
) -> Option<String> {
    steps.push(Step::Debug(format!(
        "Attempting to determine the real name of {startname}"
    )));

    let mut devname = startname.to_owned();
    if devname == "ephemeral" {
        "ephemeral0".clone_into(&mut devname);
        steps.push(Step::Debug(
            "Adjusted mount option from ephemeral to ephemeral0".to_owned(),
        ));
    }

    // Tested against the ORIGINAL name, not the `ephemeral0` rewrite.
    if is_network_device(startname) {
        return Some(startname.to_owned());
    }

    let (device_path, partition) = expand_dotted_devname(&devname);
    let orig = device_path.to_owned();
    let mut device_path = device_path.to_owned();

    // `if aliases:` -- an empty mapping skips the lookup entirely.
    if !aliases.is_empty() {
        if let Some(alias) = aliases.get(&device_path).map(super::py_str) {
            device_path = alias;
        }
        if orig != device_path {
            steps.push(Step::Debug(format!(
                "Mapped device alias {orig} to {device_path}"
            )));
        }
    }

    if is_meta_device_name(&device_path) {
        // `if not device_path` -- an empty answer from the datasource is
        // "no such device", not a device named "".
        device_path = transformer(&device_path).filter(|name| !name.is_empty())?;
        if !device_path.starts_with('/') {
            device_path = format!("/dev/{device_path}");
        }
        steps.push(Step::Debug(format!(
            "Mapped metadata name {orig} to {device_path}"
        )));
    } else if matches(DEVICE_NAME_FILTER, startname) {
        // The filter tests the dotted `startname`, which can never match it, so
        // a dotted name never gets the prefix its undotted half would. B84.
        device_path = format!("/dev/{device_path}");
    }

    let partition_path = match partition {
        None => nth_partition_for_device(root, &device_path, "1"),
        Some(partition) => {
            Some(nth_partition_for_device(root, &device_path, partition)?)
        }
    };

    if is_block_device(root, &device_path, partition_path.as_deref()) {
        return partition_path.or(Some(device_path));
    }
    None
}

/// `sanitized_devname_is_valid`.
pub fn sanitized_devname_is_valid(
    original: &str,
    sanitized: Option<&str>,
    fstab_devs: &Object,
    steps: &mut Vec<Step>,
) -> bool {
    if sanitized != Some(original) {
        steps.push(Step::Debug(format!(
            "changed {original} => {}",
            sanitized.map_or_else(|| "None".to_owned(), ToOwned::to_owned)
        )));
    }
    let Some(sanitized) = sanitized else {
        steps.push(Step::Debug(format!(
            "Ignoring nonexistent default named mount {original}"
        )));
        return false;
    };
    if let Some(line) = fstab_devs.get(sanitized) {
        steps.push(Step::Debug(format!(
            "Device {sanitized} already defined in fstab: {}",
            super::py_str(line)
        )));
        return false;
    }
    true
}

/// What `parse_fstab` returns: the lines to keep, those lines by first token,
/// and the `comment=cloudconfig` lines this module owns and will rewrite.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Fstab {
    pub lines: Vec<String>,
    pub devs: Object,
    pub removed: Vec<String>,
}

/// `parse_fstab`.
pub fn parse_fstab(root: &Path) -> Fstab {
    let mut out = Fstab::default();
    let path = super::rooted(root, FSTAB_PATH);
    if !path.exists() {
        return out;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in ci_core::pystr::split_lines(&text) {
        if line.contains(MNT_COMMENT) {
            out.removed.push(line.to_owned());
            continue;
        }
        if let Some(first) = line.split_whitespace().next() {
            out.devs
                .insert(first.to_owned(), Value::String(line.to_owned()));
            out.lines.push(line.to_owned());
        }
    }
    out
}

/// Python's `for x in value`, for the shapes `cfg.get("mounts", [])` can hold.
///
/// # Errors
/// `'<type>' object is not iterable`, which ends the module.
fn py_iter(value: &Value) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(items) => Ok(items.clone()),
        // A string iterates one character at a time, and a mapping iterates
        // its keys; both reach the "not a list" warning once per element.
        Value::String(text) => {
            Ok(text.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Value::Object(map) => {
            Ok(map.keys().map(|k| Value::String(k.clone())).collect())
        }
        other => Err(format!(
            "'{}' object is not iterable",
            super::type_name(other)
        )),
    }
}

/// `sanitize_mounts_configuration`.
///
/// # Errors
/// `list index out of range`, from an empty entry or from a `None` past the
/// end of `mount_default_fields`. B83.
pub fn sanitize_mounts_configuration(
    root: &Path,
    mounts: &Value,
    fstab_devs: &Object,
    aliases: &Object,
    default_fields: &[Value],
    transformer: Transformer<'_>,
    steps: &mut Vec<Step>,
) -> Result<Vec<Vec<Value>>, String> {
    let mut updated_lines = Vec::new();
    for line in py_iter(mounts)? {
        let Some(items) = line.as_array() else {
            steps.push(Step::Warning(format!(
                "Mount option not a list, ignoring: {}",
                super::py_str(&line)
            )));
            continue;
        };

        // `str(line[0])` on an empty entry is upstream's first IndexError.
        let start = items
            .first()
            .map(super::py_str)
            .ok_or_else(|| "list index out of range".to_owned())?;
        let sanitized = sanitize_devname(root, &start, transformer, aliases, steps);

        let mut updated: Vec<Value> = if sanitized_devname_is_valid(
            &start,
            sanitized.as_deref(),
            fstab_devs,
            steps,
        ) {
            let mut replaced = vec![Value::String(sanitized.unwrap_or_default())];
            replaced.extend(items.iter().skip(1).cloned());
            replaced
        } else {
            items.clone()
        };

        // A `None` token takes the default in the SAME position, so a line
        // longer than `mount_default_fields` raises rather than being padded.
        for index in 0..updated.len() {
            let is_null = updated.get(index).is_some_and(Value::is_null);
            if is_null {
                let default = default_fields
                    .get(index)
                    .ok_or_else(|| "list index out of range".to_owned())?;
                if let Some(slot) = updated.get_mut(index) {
                    *slot = default.clone();
                }
            } else if let Some(slot) = updated.get_mut(index) {
                *slot = Value::String(super::py_str(slot));
            }
        }

        if let Some(tail) = default_fields.get(updated.len()..) {
            updated.extend(tail.iter().cloned());
        }
        updated_lines.push(updated);
    }
    Ok(updated_lines)
}

/// `remove_nonexistent_devices`.
///
/// Walks backwards so that a dropped entry also suppresses earlier entries
/// naming the same device, then restores the original order.
///
/// # Errors
/// `list index out of range` when an entry is shorter than two tokens, which
/// happens when `mount_default_fields` was too short to pad it. B83.
pub fn remove_nonexistent_devices(
    mounts: &[Vec<Value>],
    steps: &mut Vec<Step>,
) -> Result<Vec<Vec<Value>>, String> {
    let mut kept = Vec::new();
    let mut denied: Vec<Value> = Vec::new();
    for line in mounts.iter().rev() {
        let device = line
            .first()
            .ok_or_else(|| "list index out of range".to_owned())?;
        let mountpoint = line
            .get(1)
            .ok_or_else(|| "list index out of range".to_owned())?;
        if mountpoint.is_null() || denied.contains(device) {
            steps.push(Step::Debug(format!(
                "Skipping nonexistent device named {}",
                super::py_str(device)
            )));
            denied.push(device.clone());
        } else {
            kept.push(line.clone());
        }
    }
    kept.reverse();
    Ok(kept)
}

/// `add_default_mounts_to_cfg`.
pub fn add_default_mounts_to_cfg(
    root: &Path,
    mounts: &[Vec<Value>],
    default_mount_options: &str,
    fstab_devs: &Object,
    aliases: &Object,
    transformer: Transformer<'_>,
    steps: &mut Vec<Step>,
) -> Vec<Vec<Value>> {
    let mut new_mounts = mounts.to_vec();
    let defaults = [
        [
            "ephemeral0",
            "/mnt",
            "auto",
            default_mount_options,
            "0",
            "2",
        ],
        // Upstream's own comment here is "Is this used anywhere?".
        ["swap", "none", "swap", "sw", "0", "0"],
    ];

    for default_mount in defaults {
        let Some(start) = default_mount.first().copied() else {
            continue;
        };
        let sanitized = sanitize_devname(root, start, transformer, aliases, steps);
        if !sanitized_devname_is_valid(start, sanitized.as_deref(), fstab_devs, steps) {
            continue;
        }
        let Some(sanitized) = sanitized else { continue };

        let already = mounts
            .iter()
            .any(|entry| entry.first().and_then(Value::as_str) == Some(&sanitized));
        if already {
            steps.push(Step::Debug(format!(
                "Not including {start}, already previously included"
            )));
            continue;
        }

        let mut entry: Vec<Value> = vec![Value::String(sanitized)];
        entry.extend(
            default_mount
                .iter()
                .skip(1)
                .map(|token| Value::String((*token).to_owned())),
        );
        new_mounts.push(entry);
    }
    new_mounts
}

/// `add_comment`.
///
/// # Errors
/// `list index out of range` when an entry has fewer than four tokens. B83.
pub fn add_comment(mounts: &[Vec<Value>]) -> Result<Vec<Vec<Value>>, String> {
    mounts
        .iter()
        .map(|entry| {
            let opts = entry
                .get(3)
                .ok_or_else(|| "list index out of range".to_owned())?;
            let mut out: Vec<Value> = entry.iter().take(3).cloned().collect();
            out.push(Value::String(format!(
                "{},{MNT_COMMENT}",
                super::py_str(opts)
            )));
            out.extend(entry.iter().skip(4).cloned());
            Ok(out)
        })
        .collect()
}

/// `suggested_swapsize`.
///
/// `available` is `f_frsize * f_bfree` for the filesystem the swap file will
/// live on, or `None` when upstream passed no `fsys` — the two no-filesystem
/// branches are one behaviour.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "upstream does this arithmetic in Python floats, so the port has to lose the same bits to reach the same answer"
)]
pub fn suggested_swapsize(
    memsize: u64,
    maxsize: Option<u64>,
    available: Option<u64>,
    steps: &mut Vec<Step>,
) -> u64 {
    let sugg_max = memsize.saturating_mul(2);
    let max_in = maxsize;

    let resolved_max = match available {
        Some(available) => match maxsize {
            // 25% of the filesystem, but never more than twice RAM.
            None => quarter(available).min(sugg_max),
            Some(maxsize) if maxsize as f64 > available as f64 * 0.9 => {
                ninety_percent(available)
            }
            Some(maxsize) => maxsize,
        },
        None => maxsize.unwrap_or(sugg_max),
    };

    let minsize = if memsize < 4 * GB {
        memsize
    } else if memsize < 16 * GB {
        4 * GB
    } else {
        // `round()` is banker's rounding; `f64::round` is half-away-from-zero.
        py_round((memsize as f64 / GB as f64).sqrt()) * GB
    };

    let size = minsize.min(resolved_max);

    // Every int in the log line is rendered as megabytes; `avail` and
    // `max_in` keep their non-int placeholders when they were never set.
    let mb = |value: u64| {
        format!(
            "{} MB",
            ci_core::jsonfmt::py_float(value as f64 / MB as f64)
        )
    };
    steps.push(Step::Debug(format!(
        "suggest {} swap for {} memory with '{}' disk given max={} [max={}]'",
        mb(size),
        mb(memsize),
        available.map_or_else(|| "na".to_owned(), &mb),
        max_in.map_or_else(|| "None".to_owned(), &mb),
        mb(resolved_max),
    )));
    size
}

/// `int(avail / 4)` -- true division then truncation.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "reproducing Python's float division and int() truncation"
)]
fn quarter(available: u64) -> u64 {
    (available as f64 / 4.0) as u64
}

/// `int(avail * 0.9)`.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "reproducing Python's float multiplication and int() truncation"
)]
fn ninety_percent(available: u64) -> u64 {
    (available as f64 * 0.9) as u64
}

/// Python's `round()`: halves go to the even neighbour.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a rounded square root of a byte count is non-negative and far below 2^53"
)]
fn py_round(value: f64) -> u64 {
    let floor = value.floor();
    let diff = value - floor;
    let rounded = if (diff - 0.5).abs() < f64::EPSILON {
        if (floor / 2.0).fract() == 0.0 {
            floor
        } else {
            floor + 1.0
        }
    } else if diff > 0.5 {
        floor + 1.0
    } else {
        floor
    };
    rounded as u64
}

/// Host facts the swap planner needs that no rooted path can supply, so a
/// fixture run can hand them over instead of the machine answering.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SwapEnv {
    /// `util.get_mount_info(swap_dir)[1]`. `None` is upstream subscripting
    /// `None`, which is a `TypeError`, not a missing filesystem.
    pub fstype: Option<String>,
    /// `util.kernel_version()`.
    pub kernel_version: Vec<u32>,
    /// `util.read_meminfo()["total"]`; `None` when the read raised.
    pub memtotal: Option<u64>,
    /// `os.statvfs(swap_dir).f_frsize * f_bfree`.
    pub available: Option<u64>,
}

/// `os.path.dirname`, which is a string operation and not a path one:
/// `dirname("swap.img")` is `""`, not `"."`.
fn dirname(path: &str) -> String {
    path.rsplit_once('/').map_or_else(String::new, |(head, _)| {
        if head.is_empty() {
            "/".to_owned()
        } else {
            head.to_owned()
        }
    })
}

/// `handle_swapcfg`.
///
/// Returns the steps and the path that belongs in fstab *if those steps
/// succeed* — upstream only appends the swap line when `setup_swapfile`
/// returned, so a failed creation has to drop the line too.
pub fn plan_swapcfg(
    root: &Path,
    swapcfg: &Value,
    env: &SwapEnv,
) -> (Vec<Step>, Option<String>) {
    let mut steps = Vec::new();
    let Some(cfg) = swapcfg.as_object() else {
        steps.push(Step::Warning(
            "input for swap config was not a dict.".to_owned(),
        ));
        return (steps, None);
    };

    let default_name = Value::String("/swap.img".to_owned());
    let fname_value = cfg.get("filename").unwrap_or(&default_name);
    let zero = Value::from(0);
    let size = cfg.get("size").unwrap_or(&zero);
    let maxsize = cfg.get("maxsize");

    if !(ci_config::option::py_truthy(size)
        && ci_config::option::py_truthy(fname_value))
    {
        steps.push(Step::Debug("no need to setup swap".to_owned()));
        return (steps, None);
    }
    let fname = super::py_str(fname_value);

    if super::rooted(root, &fname).exists() {
        let swaps = super::rooted(root, "/proc/swaps");
        if !swaps.exists() {
            steps.push(Step::Debug(format!(
                "swap file {fname} exists, but no /proc/swaps exists, being safe"
            )));
            return (steps, Some(fname));
        }
        let Ok(text) = std::fs::read_to_string(&swaps) else {
            steps.push(Step::Warning(format!(
                "swap file {fname} exists. Error reading /proc/swaps"
            )));
            return (steps, Some(fname));
        };
        let prefix = format!("{fname} ");
        if ci_core::pystr::split_lines(&text)
            .iter()
            .any(|line| line.starts_with(&prefix))
        {
            steps.push(Step::Debug(format!("swap file {fname} already in use")));
            return (steps, Some(fname));
        }
        steps.push(Step::Debug(format!(
            "swap file {fname} exists, but not in /proc/swaps"
        )));
    }

    match plan_setup_swapfile(&fname, size, maxsize, env, &mut steps) {
        Ok(path) => (steps, path),
        Err(error) => {
            steps.push(Step::Warning(format!("failed to setup swap: {error}")));
            (steps, None)
        }
    }
}

/// `setup_swapfile`, including the `human2bytes` conversions its caller does
/// inside the same `try`.
fn plan_setup_swapfile(
    fname: &str,
    size: &Value,
    maxsize: Option<&Value>,
    env: &SwapEnv,
    steps: &mut Vec<Step>,
) -> Result<Option<String>, String> {
    let maxsize = match maxsize {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(ci_core::human::human2bytes(text)?),
        Some(other) => Some(as_bytes(other)?),
    };

    let auto = matches!(size.as_str(), Some(text) if text.eq_ignore_ascii_case("auto"));
    let size = if auto {
        let Some(memtotal) = env.memtotal else {
            steps.push(Step::Debug(
                "Not creating swap: failed to read meminfo".to_owned(),
            ));
            return Ok(None);
        };
        steps.push(Step::EnsureDir(dirname(fname)));
        suggested_swapsize(memtotal, maxsize, env.available, steps)
    } else if let Some(text) = size.as_str() {
        ci_core::human::human2bytes(text)?
    } else {
        as_bytes(size)?
    };

    // `int(size / 2**20)`: true division then truncation, which for a
    // non-negative byte count is exactly integer division.
    #[expect(clippy::integer_division, reason = "upstream truncates here too")]
    let mib = (size / MB).to_string();
    if size == 0 {
        steps.push(Step::Debug(
            "Not creating swap: suggested size was 0".to_owned(),
        ));
        return Ok(None);
    }

    plan_create_swapfile(fname, &mib, env, steps)?;
    Ok(Some(fname.to_owned()))
}

/// A config value used as a byte count.
fn as_bytes(value: &Value) -> Result<u64, String> {
    value.as_u64().ok_or_else(|| {
        format!(
            "unsupported operand type(s) for /: '{}' and 'int'",
            super::type_name(value)
        )
    })
}

/// `create_swapfile`.
fn plan_create_swapfile(
    fname: &str,
    mib: &str,
    env: &SwapEnv,
    steps: &mut Vec<Step>,
) -> Result<(), String> {
    steps.push(Step::EnsureDir(dirname(fname)));

    let Some(fstype) = env.fstype.as_deref() else {
        return Err("'NoneType' object is not subscriptable".to_owned());
    };
    if fstype == "btrfs" {
        steps.push(Step::BtrfsPrepare(fname.to_owned()));
    }

    // fallocate on xfs corrupted swap files before 4.18, so those get dd.
    let method = if fstype == "xfs" && env.kernel_version < vec![4, 18] {
        SwapMethod::Dd
    } else {
        SwapMethod::Fallocate
    };
    steps.push(Step::Debug(creating(fname, fstype, method.name())));
    steps.push(Step::CreateSwap {
        path: fname.to_owned(),
        mib: mib.to_owned(),
        method,
        fstype: fstype.to_owned(),
    });
    steps.push(Step::ChmodSwap(fname.to_owned()));
    steps.push(Step::Mkswap(fname.to_owned()));
    Ok(())
}

/// Everything `handle` does once the mount list and the swap file are settled.
///
/// # Errors
/// The `IndexError`/`TypeError` family that a config shorter or oddly typed
/// than the schema allows walks straight into.
pub fn plan_fstab(
    mounts: &[Vec<Value>],
    fstab: &Fstab,
    uses_systemd: bool,
) -> Result<Vec<Step>, String> {
    if mounts.is_empty() {
        return Ok(vec![Step::Debug(
            "No modifications to fstab needed".to_owned(),
        )]);
    }

    let cfg_lines = mounts
        .iter()
        .map(|entry| join_tabs(entry))
        .collect::<Result<Vec<_>, _>>()?;

    let mut dirs = Vec::new();
    for entry in mounts {
        let target = entry
            .get(1)
            .ok_or_else(|| "list index out of range".to_owned())?;
        let target = target.as_str().ok_or_else(|| {
            format!(
                "'{}' object has no attribute 'startswith'",
                super::type_name(target)
            )
        })?;
        if target.starts_with('/') {
            dirs.push(target.to_owned());
        }
    }

    let mut steps: Vec<Step> =
        dirs.iter().cloned().map(Step::EnsureConfigDir).collect();

    // The comparison is done on space-normalised copies, so a line that only
    // changed its whitespace does not read as a change.
    let spaced = |line: &String| line.replace('\t', " ");
    let sadds: Vec<String> = cfg_lines.iter().map(spaced).collect();
    let sdrops: Vec<String> = fstab.removed.iter().map(spaced).collect();
    let sops: Vec<String> = sdrops
        .iter()
        .filter(|drop| !sadds.contains(drop))
        .map(|drop| format!("- {drop}"))
        .chain(
            sadds
                .iter()
                .filter(|add| !sdrops.contains(add))
                .map(|add| format!("+ {add}")),
        )
        .collect();

    let mut lines = fstab.lines.clone();
    lines.extend(cfg_lines);
    steps.push(Step::WriteFstab(format!("{}\n", lines.join("\n"))));

    let changes_made = !sops.is_empty();
    steps.push(Step::Debug(if sops.is_empty() {
        "No changes to /etc/fstab made.".to_owned()
    } else {
        format!(
            "Changes to fstab: {}",
            ci_config::repr(&Value::Array(sops.into_iter().map(Value::from).collect()))
        )
    }));

    // `activate_swap_if_needed` reads entry[2] on every entry, so a short one
    // raises here rather than being skipped.
    let mut swap = false;
    for entry in mounts {
        let kind = entry
            .get(2)
            .ok_or_else(|| "list index out of range".to_owned())?;
        if kind.as_str() == Some("swap") {
            swap = true;
        }
    }
    if swap {
        steps.push(Step::SwapOn);
    }
    steps.push(Step::MountAll {
        daemon_reload: uses_systemd,
        changes_made,
        dirs,
    });
    Ok(steps)
}

/// `"\t".join(entry)`, which needs every token to be a string by now.
fn join_tabs(entry: &[Value]) -> Result<String, String> {
    let mut out = String::new();
    for (index, token) in entry.iter().enumerate() {
        let text = token.as_str().ok_or_else(|| {
            format!(
                "sequence item {index}: expected str instance, {} found",
                super::type_name(token)
            )
        })?;
        if index > 0 {
            out.push('\t');
        }
        out.push_str(text);
    }
    Ok(out)
}

/// The four passes `handle` makes over the `mounts` config, in order.
///
/// # Errors
/// The `IndexError` family; the steps decided before it still belong in the
/// log, so the caller runs them before reporting.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per thing upstream reads from `cloud` or `cfg`"
)]
pub fn plan_mounts(
    root: &Path,
    mounts: &Value,
    fstab_devs: &Object,
    aliases: &Object,
    default_fields: &[Value],
    default_mount_options: &str,
    transformer: Transformer<'_>,
    steps: &mut Vec<Step>,
) -> Result<Vec<Vec<Value>>, String> {
    let updated = sanitize_mounts_configuration(
        root,
        mounts,
        fstab_devs,
        aliases,
        default_fields,
        transformer,
        steps,
    )?;
    let updated = add_default_mounts_to_cfg(
        root,
        &updated,
        default_mount_options,
        fstab_devs,
        aliases,
        transformer,
        steps,
    );
    let updated = remove_nonexistent_devices(&updated, steps)?;
    add_comment(&updated)
}

/// `handle`.
///
/// # Errors
/// Whatever aborted upstream: the `IndexError` family from a mis-shaped
/// `mounts` entry, or a failed `mount -a`.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let uses_systemd = super::uses_systemd(args.root);
    let default_mount_options = if uses_systemd {
        "defaults,nofail,x-systemd.after=cloud-init-network.service,_netdev"
    } else {
        "defaults,nobootwait"
    };

    let hardcoded = vec![
        Value::Null,
        Value::Null,
        Value::String("auto".to_owned()),
        Value::String(default_mount_options.to_owned()),
        Value::String("0".to_owned()),
        Value::String("2".to_owned()),
    ];
    // A non-list `mount_default_fields` would be indexed elementwise upstream;
    // the port falls back instead, which is the one shape it does not mirror.
    // Deviation 150.
    let default_fields = args
        .cfg
        .get("mount_default_fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or(hardcoded);

    let mounts = args
        .cfg
        .get("mounts")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut steps = vec![Step::Debug(format!(
        "mounts configuration is {}",
        super::py_str(&mounts)
    ))];

    let fstab = parse_fstab(args.root);
    let aliases = args
        .cfg
        .get("device_aliases")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // `cloud.device_name_to_device` is a datasource override and none of the
    // four that exist upstream is ported, so this answers like the base class.
    // Deviation 149.
    let transformer = |_: &str| -> Option<String> { None };

    let updated = plan_mounts(
        args.root,
        &mounts,
        &fstab.devs,
        &aliases,
        &default_fields,
        default_mount_options,
        &transformer,
        &mut steps,
    );
    let mut updated = match updated {
        Ok(updated) => updated,
        Err(error) => return abort(&steps, args, error),
    };

    let swapcfg = args
        .cfg
        .get("swap")
        .cloned()
        .unwrap_or_else(|| Value::Object(Object::new()));
    let env = swap_env(args, &swapcfg);
    let (swap_steps, swapfile) = plan_swapcfg(args.root, &swapcfg, &env);

    run(&steps, args)?;
    // Upstream wraps the whole swap attempt in one `except Exception`, so a
    // failure here is a warning and the fstab line is simply not added.
    let swapfile = match run(&swap_steps, args) {
        Ok(()) => swapfile,
        Err(error) => {
            args.warning(SOURCE, &format!("failed to setup swap: {error}"));
            None
        }
    };
    if let Some(path) = swapfile {
        let mut entry = vec![Value::String(path)];
        entry.extend(
            ["none", "swap", "sw", "0", "0"]
                .into_iter()
                .map(|token| Value::String(token.to_owned())),
        );
        updated.push(entry);
    }

    match plan_fstab(&updated, &fstab, uses_systemd) {
        Ok(steps) => run(&steps, args),
        Err(error) => Err(error),
    }
}

/// Upstream's side effects up to the raise are real, so the log lines decided
/// before an error still reach the log before it is reported.
fn abort(steps: &[Step], args: &mut Args<'_>, error: String) -> Result<(), String> {
    run(steps, args)?;
    Err(error)
}

/// The host facts `plan_swapcfg` cannot read through a rooted path.
fn swap_env(args: &Args<'_>, swapcfg: &Value) -> SwapEnv {
    if args.root != Path::new("/") {
        return SwapEnv::default();
    }
    let fname = swapcfg
        .as_object()
        .and_then(|cfg| cfg.get("filename"))
        .map_or_else(|| "/swap.img".to_owned(), super::py_str);
    let swap_dir = dirname(&fname);
    SwapEnv {
        fstype: ci_sys::mount::get_mount_info(&swap_dir).map(|info| info.fs_type),
        kernel_version: ci_core::sysinfo::kernel_version().unwrap_or_default(),
        memtotal: ci_core::sysinfo::read_meminfo(Path::new("/proc/meminfo"))
            .ok()
            .and_then(|info| info.total),
        available: statvfs_available(&swap_dir),
    }
}

/// `os.statvfs(dir).f_frsize * f_bfree`.
///
/// `ci-sys` forbids `unsafe` and std has no `statvfs`, so this asks coreutils
/// for the same two numbers; the product is identical. Deviation 148.
fn statvfs_available(dir: &str) -> Option<u64> {
    let output = ci_sys::subp::Subp::new(["stat", "-f", "-c", "%S %f", dir])
        .check()
        .ok()?;
    let text = output.stdout_lossy();
    let (frsize, bfree) = text.trim().split_once(' ')?;
    let frsize: u64 = frsize.parse().ok()?;
    let bfree: u64 = bfree.parse().ok()?;
    Some(frsize.saturating_mul(bfree))
}

/// Carry out a plan. Nothing that changes the machine runs unless the root is
/// `/`, so a fixture tree cannot rewrite this host's fstab or swap.
fn run(steps: &[Step], args: &mut Args<'_>) -> Result<(), String> {
    let live = args.root == Path::new("/");
    for step in steps {
        match step {
            Step::Debug(message) => args.debug(SOURCE, message),
            Step::Warning(message) => args.warning(SOURCE, message),
            Step::Info(message) => args.info(SOURCE, message),
            Step::EnsureDir(dir) => {
                std::fs::create_dir_all(super::rooted(args.root, dir))
                    .map_err(|error| format!("{dir}: {error}"))?;
            }
            Step::EnsureConfigDir(dir) => {
                if std::fs::create_dir_all(super::rooted(args.root, dir)).is_err() {
                    args.warning(
                        SOURCE,
                        &format!("Failed to make '{dir}' config-mount"),
                    );
                }
            }
            Step::BtrfsPrepare(path) => {
                if live {
                    subp(["truncate", "-s", "0", path])?;
                    subp(["chattr", "+C", path])?;
                }
            }
            Step::CreateSwap {
                path,
                mib,
                method,
                fstype,
            } => {
                if live {
                    create_swap(args, path, mib, *method, fstype)?;
                }
            }
            Step::ChmodSwap(path) => {
                let target = super::rooted(args.root, path);
                if target.exists() {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &target,
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .map_err(|error| format!("{path}: {error}"))?;
                }
            }
            Step::Mkswap(path) => {
                if live {
                    subp(["mkswap", path]).inspect_err(|_| {
                        let _ = std::fs::remove_file(super::rooted(args.root, path));
                    })?;
                }
            }
            Step::WriteFstab(contents) => {
                super::write_file(
                    &super::rooted(args.root, FSTAB_PATH),
                    contents.as_bytes(),
                )?;
            }
            Step::SwapOn => {
                if live {
                    subp(["swapon", "-a"])?;
                }
            }
            Step::MountAll {
                daemon_reload,
                changes_made,
                dirs,
            } => {
                if live && mount_needed(*changes_made, dirs) {
                    subp(["mount", "-a"])?;
                    if *daemon_reload {
                        subp(["systemctl", "daemon-reload"])?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// `mount_if_needed`'s test: always after a change, otherwise only when some
/// configured directory is not mounted yet.
fn mount_needed(changes_made: bool, dirs: &[String]) -> bool {
    if changes_made {
        return true;
    }
    let mounted = ci_sys::mount::mounts();
    dirs.iter().any(|dir| !mounted.contains_key(Path::new(dir)))
}

/// `create_swap`, with upstream's fallocate-to-dd fallback.
fn create_swap(
    args: &mut Args<'_>,
    path: &str,
    mib: &str,
    method: SwapMethod,
    fstype: &str,
) -> Result<(), String> {
    // The first attempt's "Creating swapfile" line is logged from the plan,
    // since the method it will use is already settled there.
    if method == SwapMethod::Fallocate {
        match swap_command(path, mib, SwapMethod::Fallocate) {
            Ok(()) => return Ok(()),
            Err(error) => {
                failed_attempt(args, path, mib, "fallocate", &error);
                args.info(
                    SOURCE,
                    "fallocate swap creation failed, will attempt with dd",
                );
                args.debug(SOURCE, &creating(path, fstype, "dd"));
            }
        }
    }
    swap_command(path, mib, SwapMethod::Dd)
        .inspect_err(|error| failed_attempt(args, path, mib, "dd", error))
}

/// Upstream logs the failure and deletes the part-written file inside
/// `create_swap`, before the caller decides whether to retry.
fn failed_attempt(
    args: &mut Args<'_>,
    path: &str,
    mib: &str,
    method: &str,
    error: &str,
) {
    args.info(SOURCE, &create_failure(path, mib, method, error));
    let _ = std::fs::remove_file(super::rooted(args.root, path));
}

/// `create_swap`'s opening debug line, logged once per attempt.
fn creating(path: &str, fstype: &str, method: &str) -> String {
    format!("Creating swapfile in '{path}' on fstype '{fstype}' using '{method}'")
}

fn create_failure(path: &str, mib: &str, method: &str, error: &str) -> String {
    format!("Failed to create swapfile '{path}' of size {mib}MB via {method}: {error}")
}

fn swap_command(path: &str, mib: &str, method: SwapMethod) -> Result<(), String> {
    match method {
        SwapMethod::Fallocate => subp(["fallocate", "-l", &format!("{mib}M"), path]),
        SwapMethod::Dd => subp([
            "dd",
            "if=/dev/zero",
            &format!("of={path}"),
            "bs=1M",
            &format!("count={mib}"),
        ]),
    }
}

fn subp<'a, I: IntoIterator<Item = &'a str>>(argv: I) -> Result<(), String> {
    ci_sys::subp::Subp::new(argv)
        .check()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn strs(tokens: &[&str]) -> Vec<Value> {
        tokens
            .iter()
            .map(|t| Value::String((*t).to_owned()))
            .collect()
    }

    fn debugs(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Debug(message) | Step::Warning(message) => Some(message.as_str()),
                _ => None,
            })
            .collect()
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    /// A tree with one whole disk, one partition of it, and the `/sys/block`
    /// entries that make both of them real to `_is_block_device`.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in ["dev", "sys/block/sda/sda1", "sys/block/sdb"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        for path in ["dev/sda", "dev/sda1", "dev/sdb"] {
            std::fs::write(root.join(path), b"").unwrap();
        }
        dir
    }

    #[test]
    fn expand_dotted_devname_splits_on_the_last_dot() {
        assert_eq!(expand_dotted_devname("sda"), ("sda", None));
        assert_eq!(expand_dotted_devname("sda.1"), ("sda", Some("1")));
        assert_eq!(expand_dotted_devname("a.b.2"), ("a.b", Some("2")));
        assert_eq!(expand_dotted_devname("sda."), ("sda", Some("")));
        assert_eq!(expand_dotted_devname(""), ("", None));
    }

    #[test]
    fn meta_device_names_are_the_metadata_aliases() {
        for name in ["ami", "root", "swap", "ephemeral", "ephemeral0", "ebs1"] {
            assert!(is_meta_device_name(name), "{name}");
        }
        // A colon makes it a network mount, not a metadata name.
        for name in ["ephemeral0:x", "sda", "", "Root", "ebs:1"] {
            assert!(!is_meta_device_name(name), "{name}");
        }
    }

    #[test]
    fn network_names_need_a_colon_with_something_before_it() {
        assert!(is_network_device("server:/path"));
        assert!(is_network_device("a:"));
        assert!(is_network_device("a:b\nc"));
        assert!(!is_network_device(":/path"));
        assert!(!is_network_device("nocolon"));
        // `.` does not cross a newline in either language.
        assert!(!is_network_device("a\nb:c"));
    }

    #[test]
    fn the_device_filter_accepts_a_trailing_newline_like_python() {
        for name in ["sda", "sda1", "xvda", "xvdb1", "hda", "vdd1", "sr0", "sr12"] {
            assert!(matches(DEVICE_NAME_FILTER, name), "{name}");
        }
        // Python's `$` also matches just before a final newline.
        assert!(matches(DEVICE_NAME_FILTER, "sda1\n"));
        for name in ["sda1\nx", "SDA1", "xxvda", "nvme0n1", "", "sda1 "] {
            assert!(!matches(DEVICE_NAME_FILTER, name), "{name}");
        }
    }

    #[test]
    fn a_whole_disk_resolves_and_a_missing_one_does_not() {
        let dir = fixture();
        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(dir.path(), "sdb", &none, &Object::new(), &mut steps),
            Some("/dev/sdb".to_owned())
        );
        assert_eq!(
            debugs(&steps),
            ["Attempting to determine the real name of sdb"]
        );

        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(dir.path(), "sdz", &none, &Object::new(), &mut steps),
            None
        );
    }

    #[test]
    fn a_disk_with_a_first_partition_resolves_to_the_partition() {
        let dir = fixture();
        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(dir.path(), "sda", &none, &Object::new(), &mut steps),
            Some("/dev/sda1".to_owned())
        );
    }

    /// The filter is applied to the dotted `startname`, which can never match
    /// it, so a dotted name is never given the `/dev/` prefix.
    #[test]
    fn a_dotted_name_never_reaches_dev() {
        let dir = fixture();
        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(dir.path(), "sda.1", &none, &Object::new(), &mut steps),
            None
        );
        // An alias supplies the full path the filter would have added.
        let mut aliases = Object::new();
        aliases.insert("sda".to_owned(), Value::String("/dev/sda".to_owned()));
        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(dir.path(), "sda.1", &none, &aliases, &mut steps),
            Some("/dev/sda1".to_owned())
        );
        assert!(debugs(&steps).contains(&"Mapped device alias sda to /dev/sda"));
    }

    #[test]
    fn ephemeral_is_rewritten_then_handed_to_the_datasource() {
        let dir = fixture();
        let mut steps = Vec::new();
        let transformer = |name: &str| {
            assert_eq!(name, "ephemeral0");
            Some("sdb".to_owned())
        };
        assert_eq!(
            sanitize_devname(
                dir.path(),
                "ephemeral",
                &transformer,
                &Object::new(),
                &mut steps
            ),
            Some("/dev/sdb".to_owned())
        );
        assert_eq!(
            debugs(&steps),
            [
                "Attempting to determine the real name of ephemeral",
                "Adjusted mount option from ephemeral to ephemeral0",
                "Mapped metadata name ephemeral0 to /dev/sdb",
            ]
        );
    }

    #[test]
    fn an_empty_answer_from_the_datasource_is_no_device() {
        let dir = fixture();
        let mut steps = Vec::new();
        let empty = |_: &str| Some(String::new());
        assert_eq!(
            sanitize_devname(
                dir.path(),
                "ephemeral0",
                &empty,
                &Object::new(),
                &mut steps
            ),
            None
        );
    }

    #[test]
    fn a_network_mount_is_passed_through_untouched() {
        let dir = fixture();
        let mut steps = Vec::new();
        assert_eq!(
            sanitize_devname(
                dir.path(),
                "server:/export",
                &none,
                &Object::new(),
                &mut steps
            ),
            Some("server:/export".to_owned())
        );
    }

    #[test]
    fn parse_fstab_separates_the_lines_this_module_owns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/fstab"),
            "/dev/sda1 / ext4 defaults 0 1\n\
             \n\
             /dev/sdb /mnt auto defaults,comment=cloudconfig 0 2\n\
             /dev/sdc /srv ext4 defaults 0 2\n",
        )
        .unwrap();

        let fstab = parse_fstab(dir.path());
        assert_eq!(
            fstab.lines,
            [
                "/dev/sda1 / ext4 defaults 0 1",
                "/dev/sdc /srv ext4 defaults 0 2",
            ]
        );
        assert_eq!(
            fstab.removed,
            ["/dev/sdb /mnt auto defaults,comment=cloudconfig 0 2"]
        );
        assert_eq!(fstab.devs.len(), 2);
        assert!(fstab.devs.contains_key("/dev/sda1"));
    }

    #[test]
    fn a_missing_fstab_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(parse_fstab(dir.path()), Fstab::default());
    }

    /// The six-field defaults `handle` builds when the config has none.
    fn defaults() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Null,
            Value::String("auto".to_owned()),
            Value::String("defaults,nobootwait".to_owned()),
            Value::String("0".to_owned()),
            Value::String("2".to_owned()),
        ]
    }

    fn sanitize(mounts: &Value, defaults: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        let dir = fixture();
        let mut steps = Vec::new();
        sanitize_mounts_configuration(
            dir.path(),
            mounts,
            &Object::new(),
            &Object::new(),
            defaults,
            &none,
            &mut steps,
        )
    }

    #[test]
    fn a_short_entry_is_padded_from_the_defaults() {
        let mounts = Value::Array(vec![Value::Array(strs(&["sdb"]))]);
        let got = sanitize(&mounts, &defaults()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][0], Value::String("/dev/sdb".to_owned()));
        // `default_fields[1]` is None, so the mountpoint stays unset and
        // `remove_nonexistent_devices` will drop the whole entry.
        assert_eq!(got[0][1], Value::Null);
        assert_eq!(got[0].len(), 6);
    }

    #[test]
    fn an_empty_entry_is_an_index_error() {
        let mounts = Value::Array(vec![Value::Array(Vec::new())]);
        assert_eq!(
            sanitize(&mounts, &defaults()),
            Err("list index out of range".to_owned())
        );
    }

    #[test]
    fn a_null_past_the_end_of_the_defaults_is_an_index_error() {
        let mut entry = strs(&["a", "b", "c", "d", "e", "f"]);
        entry.push(Value::Null);
        let mounts = Value::Array(vec![Value::Array(entry)]);
        assert_eq!(
            sanitize(&mounts, &defaults()),
            Err("list index out of range".to_owned())
        );
    }

    #[test]
    fn a_seven_field_entry_without_a_null_is_left_alone() {
        let entry = strs(&["a", "b", "c", "d", "e", "f", "g"]);
        let mounts = Value::Array(vec![Value::Array(entry.clone())]);
        assert_eq!(sanitize(&mounts, &defaults()).unwrap(), vec![entry]);
    }

    #[test]
    fn empty_defaults_pad_nothing() {
        let mounts = Value::Array(vec![Value::Array(strs(&["sdb"]))]);
        let got = sanitize(&mounts, &[]).unwrap();
        assert_eq!(got, vec![strs(&["/dev/sdb"])]);
    }

    #[test]
    fn a_non_list_entry_is_warned_about_and_skipped() {
        let mounts = Value::Array(vec![Value::String("sdb".to_owned())]);
        let dir = fixture();
        let mut steps = Vec::new();
        let got = sanitize_mounts_configuration(
            dir.path(),
            &mounts,
            &Object::new(),
            &Object::new(),
            &defaults(),
            &none,
            &mut steps,
        )
        .unwrap();
        assert!(got.is_empty());
        assert_eq!(
            steps,
            [Step::Warning(
                "Mount option not a list, ignoring: sdb".to_owned()
            )]
        );
    }

    /// `mounts: "ab"` iterates characters, so each one reaches the warning.
    #[test]
    fn a_string_config_iterates_its_characters() {
        let mounts = Value::String("ab".to_owned());
        let dir = fixture();
        let mut steps = Vec::new();
        let got = sanitize_mounts_configuration(
            dir.path(),
            &mounts,
            &Object::new(),
            &Object::new(),
            &defaults(),
            &none,
            &mut steps,
        )
        .unwrap();
        assert!(got.is_empty());
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn a_scalar_config_is_not_iterable() {
        let mounts = Value::Null;
        assert_eq!(
            sanitize(&mounts, &defaults()),
            Err("'NoneType' object is not iterable".to_owned())
        );
    }

    #[test]
    fn an_entry_without_a_mountpoint_is_dropped() {
        let mounts = vec![
            strs(&["/dev/sdb", "/mnt", "auto", "defaults", "0", "2"]),
            vec![
                Value::String("/dev/sdc".to_owned()),
                Value::Null,
                Value::String("auto".to_owned()),
            ],
        ];
        let mut steps = Vec::new();
        let got = remove_nonexistent_devices(&mounts, &mut steps).unwrap();
        assert_eq!(got, vec![mounts[0].clone()]);
        assert_eq!(
            debugs(&steps),
            ["Skipping nonexistent device named /dev/sdc"]
        );
    }

    /// The walk is backwards with a denylist, so a bad entry also suppresses
    /// an EARLIER good entry naming the same device.
    #[test]
    fn a_later_bad_entry_suppresses_an_earlier_good_one() {
        let mounts = vec![
            strs(&["/dev/sdb", "/mnt", "auto", "defaults", "0", "2"]),
            vec![Value::String("/dev/sdb".to_owned()), Value::Null],
        ];
        let mut steps = Vec::new();
        assert!(remove_nonexistent_devices(&mounts, &mut steps)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_entry_shorter_than_two_tokens_is_an_index_error() {
        let mounts = vec![strs(&["sda1"])];
        let mut steps = Vec::new();
        assert_eq!(
            remove_nonexistent_devices(&mounts, &mut steps),
            Err("list index out of range".to_owned())
        );
    }

    #[test]
    fn add_comment_tags_the_options_field() {
        let mounts = vec![strs(&["a", "b", "c", "d", "e", "f"])];
        assert_eq!(
            add_comment(&mounts).unwrap(),
            vec![strs(&["a", "b", "c", "d,comment=cloudconfig", "e", "f"])]
        );
    }

    #[test]
    fn add_comment_needs_a_fourth_field() {
        assert_eq!(
            add_comment(&[strs(&["a"])]),
            Err("list index out of range".to_owned())
        );
    }

    #[test]
    fn defaults_are_added_only_when_the_device_is_real() {
        let dir = fixture();
        let mut steps = Vec::new();
        let transformer = |name: &str| (name == "ephemeral0").then(|| "sdb".to_owned());
        let got = add_default_mounts_to_cfg(
            dir.path(),
            &[],
            "defaults,nobootwait",
            &Object::new(),
            &Object::new(),
            &transformer,
            &mut steps,
        );
        // `ephemeral0` resolves; the `swap` default does not, because the
        // transformer has no answer for it.
        assert_eq!(
            got,
            vec![strs(&[
                "/dev/sdb",
                "/mnt",
                "auto",
                "defaults,nobootwait",
                "0",
                "2"
            ])]
        );
        assert!(
            debugs(&steps).contains(&"Ignoring nonexistent default named mount swap")
        );
    }

    #[test]
    fn a_default_already_in_the_config_is_not_added_twice() {
        let dir = fixture();
        let mut steps = Vec::new();
        let transformer = |_: &str| Some("sdb".to_owned());
        let existing = vec![strs(&["/dev/sdb", "/data", "auto", "defaults", "0", "2"])];
        let got = add_default_mounts_to_cfg(
            dir.path(),
            &existing,
            "defaults,nobootwait",
            &Object::new(),
            &Object::new(),
            &transformer,
            &mut steps,
        );
        assert_eq!(got, existing);
        assert!(debugs(&steps)
            .contains(&"Not including ephemeral0, already previously included"));
    }

    #[test]
    fn a_device_already_in_fstab_is_left_to_the_image() {
        let mut fstab_devs = Object::new();
        fstab_devs.insert(
            "/dev/sdb".to_owned(),
            Value::String("/dev/sdb /srv ext4 defaults 0 2".to_owned()),
        );
        let mut steps = Vec::new();
        assert!(!sanitized_devname_is_valid(
            "sdb",
            Some("/dev/sdb"),
            &fstab_devs,
            &mut steps
        ));
        assert_eq!(
            debugs(&steps),
            [
                "changed sdb => /dev/sdb",
                "Device /dev/sdb already defined in fstab: /dev/sdb /srv ext4 defaults 0 2",
            ]
        );
    }

    /// Every number and every rendered log line here came from running the
    /// packaged `suggested_swapsize` on the same inputs.
    #[test]
    fn suggested_swapsize_matches_upstream_without_a_filesystem() {
        let cases: [(u64, Option<u64>, u64, &str); 6] = [
            (2 * GB, None, 2 * GB,
             "suggest 2048.0 MB swap for 2048.0 MB memory with 'na' disk given max=None [max=4096.0 MB]'"),
            (2 * GB, Some(GB), GB,
             "suggest 1024.0 MB swap for 2048.0 MB memory with 'na' disk given max=1024.0 MB [max=1024.0 MB]'"),
            (8 * GB, None, 4 * GB,
             "suggest 4096.0 MB swap for 8192.0 MB memory with 'na' disk given max=None [max=16384.0 MB]'"),
            (32 * GB, None, 6 * GB,
             "suggest 6144.0 MB swap for 32768.0 MB memory with 'na' disk given max=None [max=65536.0 MB]'"),
            (64 * GB, None, 8 * GB,
             "suggest 8192.0 MB swap for 65536.0 MB memory with 'na' disk given max=None [max=131072.0 MB]'"),
            (0, None, 0,
             "suggest 0.0 MB swap for 0.0 MB memory with 'na' disk given max=None [max=0.0 MB]'"),
        ];
        for (memsize, maxsize, want, log) in cases {
            let mut steps = Vec::new();
            assert_eq!(suggested_swapsize(memsize, maxsize, None, &mut steps), want);
            assert_eq!(debugs(&steps), [log], "mem={memsize}");
        }
    }

    /// A byte count that is not a whole number of megabytes still has to print
    /// the way Python's `repr` prints the quotient.
    #[test]
    fn suggested_swapsize_prints_a_tiny_memory_in_exponent_form() {
        let mut steps = Vec::new();
        assert_eq!(suggested_swapsize(1, None, None, &mut steps), 1);
        assert_eq!(
            debugs(&steps),
            ["suggest 9.5367431640625e-07 MB swap for 9.5367431640625e-07 MB memory \
              with 'na' disk given max=None [max=1.9073486328125e-06 MB]'"]
        );
    }

    #[test]
    fn suggested_swapsize_matches_upstream_with_a_filesystem() {
        let avail = 14_167_855_104_u64;
        let cases: [(u64, Option<u64>, u64, &str); 3] = [
            (2 * GB, None, 2 * GB,
             "suggest 2048.0 MB swap for 2048.0 MB memory with '13511.51953125 MB' disk given max=None [max=3377.8798828125 MB]'"),
            (2 * GB, Some(1_000_000_000_000_000), 2 * GB,
             "suggest 2048.0 MB swap for 2048.0 MB memory with '13511.51953125 MB' disk given max=953674316.40625 MB [max=12160.367577552795 MB]'"),
            (2 * GB, Some(GB), GB,
             "suggest 1024.0 MB swap for 2048.0 MB memory with '13511.51953125 MB' disk given max=1024.0 MB [max=1024.0 MB]'"),
        ];
        for (memsize, maxsize, want, log) in cases {
            let mut steps = Vec::new();
            assert_eq!(
                suggested_swapsize(memsize, maxsize, Some(avail), &mut steps),
                want
            );
            assert_eq!(debugs(&steps), [log], "mem={memsize} max={maxsize:?}");
        }
    }

    #[test]
    fn py_round_sends_halves_to_the_even_neighbour() {
        assert_eq!(py_round(0.5), 0);
        assert_eq!(py_round(1.5), 2);
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
        assert_eq!(py_round(4.472_136), 4);
        assert_eq!(py_round(5.477_226), 5);
    }
}
