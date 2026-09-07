//! `util.find_devs_with` and `util.mount_cb`: finding a block device and
//! reading it.
//!
//! Both exist for one job — a datasource whose seed arrives on a removable
//! device (an Azure provisioning ISO, a config drive) has to locate that device
//! and read a file off it before the network is up. Upstream reaches for
//! `blkid` and `mount(8)`, and so does this; there is no way to do either from
//! a process that refuses `unsafe`.
//!
//! Only the Linux paths are ported (deviation 113); upstream's four BSD
//! variants of `find_devs_with` are not.
//!
//! Nothing here logs. `ci-sys` is the root of the crate graph and `ci-log` sits
//! above it, so the diagnostics upstream writes from inside these functions are
//! handed back to the caller instead — the same arrangement `ci-net` uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::subp::{self, Subp};

/// One entry of `/proc/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub fstype: String,
    pub mountpoint: PathBuf,
    pub opts: String,
}

/// `util.mounts`, keyed by device.
///
/// Upstream falls back to parsing `mount(8)` output when `/proc/mounts` is
/// absent; that fallback is not ported, because a Linux system without `/proc`
/// cannot get this far anyway. An unreadable `/proc/mounts` is an empty map,
/// not an error, which is what upstream's `except (IOError, OSError)` amounts
/// to.
#[must_use]
pub fn mounts() -> BTreeMap<PathBuf, Mount> {
    let text = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    parse_proc_mounts(&text)
}

fn parse_proc_mounts(text: &str) -> BTreeMap<PathBuf, Mount> {
    let mut mounted = BTreeMap::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        // Upstream requires exactly six fields and skips anything else.
        let [dev, mp, fstype, opts, _freq, _passno] = words[..] else {
            continue;
        };
        mounted.insert(
            PathBuf::from(unescape_octal(dev)),
            Mount {
                fstype: fstype.to_owned(),
                // Upstream only undoes `\040`; the kernel also escapes tab,
                // newline and backslash, so all four are undone here. A mount
                // point with a tab in it would otherwise never match.
                mountpoint: PathBuf::from(unescape_octal(mp)),
                opts: opts.to_owned(),
            },
        );
    }
    mounted
}

/// `\040` and friends, as the kernel writes them in `/proc/mounts`.
fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some([b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7']) =
            bytes.get(i..i + 4)
        {
            out.push(char::from((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0')));
            i += 4;
        } else {
            // Advance by whole characters so the slice stays on a boundary.
            let Some(ch) = field[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// What `util.get_mount_info` answers: which device a path lives on.
///
/// Upstream returns a 3-tuple, or a 4-tuple when `get_mnt_opts` is set. Both
/// are this one struct; `opts` is empty for the `/etc/mtab` fallback, which
/// has no options column to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub devpth: String,
    pub fs_type: String,
    pub mount_point: String,
    pub opts: String,
}

/// Why `parse_mount_info` gave up on a `mountinfo` file.
///
/// Upstream logs these at debug and returns `None`; `ci-sys` has no logger, so
/// they come back to the caller instead. `Display` reproduces the message
/// upstream writes, typo and all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountInfoError {
    TooFewColumns {
        line: usize,
        columns: usize,
        text: String,
    },
    NoSeparator {
        line: usize,
        text: String,
    },
    /// The `line` here is upstream's, and upstream's is wrong: by the time it
    /// formats this message the loop variable has been overwritten by
    /// `parts.index("-")`, so it reports the *column* the separator was found
    /// at, plus one. See docs/COMPAT.md.
    TooFewAfterSeparator {
        line: usize,
        text: String,
    },
}

impl std::fmt::Display for MountInfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewColumns {
                line,
                columns,
                text,
            } => write!(f, "Line {line} has two few columns ({columns}): {text}"),
            Self::NoSeparator { line, text } => {
                write!(f, "Did not find column named '-' in line {line}: {text}")
            }
            Self::TooFewAfterSeparator { line, text } => {
                write!(f, "Too few columns after '-' column in line {line}: {text}")
            }
        }
    }
}

/// `util.parse_mount_info`.
///
/// Walks every line and keeps the *deepest* mount point that is a prefix of
/// `path`, which is how a path inside a bind mount or a btrfs subvolume
/// resolves to the device actually backing it rather than to `/`.
///
/// A malformed line abandons the whole file rather than being skipped:
/// upstream's reasoning is that parsing past it could return a confidently
/// wrong device, and a wrong device here is something a caller might go on to
/// resize or reformat.
///
/// Upstream's `get_mnt_opts` flag chooses between a 3- and a 4-tuple. There is
/// no flag here because [`MountInfo`] always carries the options, and because
/// the flag's other job -- a truthiness guard on the fields -- can never fire:
/// `split()` yields no empty strings, so a match at all is a match with every
/// field populated.
///
/// # Errors
/// The three shapes of malformed line, each of which upstream turns into
/// `None` after a debug line.
pub fn parse_mount_info(
    path: &str,
    lines: &[&str],
) -> Result<Option<MountInfo>, MountInfoError> {
    let path_elements: Vec<&str> = path.split('/').filter(|e| !e.is_empty()).collect();
    let mut found: Option<MountInfo> = None;
    let mut match_elements: Option<Vec<&str>> = None;

    for (index, line) in lines.iter().enumerate() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 10 {
            return Err(MountInfoError::TooFewColumns {
                line: index + 1,
                columns: parts.len(),
                text: (*line).to_owned(),
            });
        }

        // The length check above guarantees columns 4 and 5 are there.
        let (Some(mount_point), Some(mount_options)) = (parts.get(4), parts.get(5))
        else {
            continue;
        };
        let mount_point_elements: Vec<&str> =
            mount_point.split('/').filter(|e| !e.is_empty()).collect();

        if mount_point_elements.len() > path_elements.len() {
            continue;
        }
        let shared = mount_point_elements.len().min(path_elements.len());
        if mount_point_elements.get(..shared) != path_elements.get(..shared) {
            continue;
        }
        // Strictly greater, so a later mount at the same depth replaces an
        // earlier one.
        if match_elements
            .as_ref()
            .is_some_and(|seen| seen.len() > mount_point_elements.len())
        {
            continue;
        }

        let Some(separator) = parts.iter().position(|p| *p == "-") else {
            return Err(MountInfoError::NoSeparator {
                line: index + 1,
                text: (*line).to_owned(),
            });
        };
        let (Some(fs_type), Some(devpth)) =
            (parts.get(separator + 1), parts.get(separator + 2))
        else {
            return Err(MountInfoError::TooFewAfterSeparator {
                line: separator + 1,
                text: (*line).to_owned(),
            });
        };

        found = Some(MountInfo {
            devpth: (*devpth).to_owned(),
            fs_type: (*fs_type).to_owned(),
            mount_point: (*mount_point).to_owned(),
            opts: (*mount_options).to_owned(),
        });
        match_elements = Some(mount_point_elements);
    }

    // Upstream ends with a chain of truthiness tests on the four fields; see
    // the note on `get_mnt_opts` above for why it is dropped here.
    Ok(found)
}

/// `util.parse_mtab`: the pre-`mountinfo` fallback, which matches the mount
/// point exactly rather than by prefix and so cannot see through a bind mount.
#[must_use]
pub fn parse_mtab(path: &str, text: &str) -> Option<MountInfo> {
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `line.split()[:3]` unpacked into three names, so a line with fewer
        // than three fields is a ValueError that escapes the function.
        let [devpth, mount_point, fs_type, ..] = fields.as_slice() else {
            continue;
        };
        if *mount_point == path {
            return Some(MountInfo {
                devpth: (*devpth).to_owned(),
                fs_type: (*fs_type).to_owned(),
                mount_point: (*mount_point).to_owned(),
                opts: String::new(),
            });
        }
    }
    None
}

/// `util.get_mount_info`.
///
/// `/proc/self/mountinfo` rather than upstream's `/proc/<pid>/mountinfo`;
/// they are the same file for the calling process.
///
/// Upstream's third fallback, `parse_mount`, shells out to `mount(8)` and is
/// BSD-only in practice; it is not ported (deviation 113 covers the family).
#[must_use]
pub fn get_mount_info(path: &str) -> Option<MountInfo> {
    if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") {
        let lines: Vec<&str> = text.lines().collect();
        return parse_mount_info(path, &lines).ok().flatten();
    }
    let text = std::fs::read_to_string("/etc/mtab").ok()?;
    parse_mtab(path, &text)
}

/// `util.has_mount_opt(path, opt)`.
///
/// Upstream unpacks a 4-tuple, so a path it cannot place raises there; here it
/// is `false`, which is what both callers -- the temp-directory chooser and
/// `Distro.get_tmp_exec_path` -- would have wanted anyway.
#[must_use]
pub fn has_mount_opt(path: &str, opt: &str) -> bool {
    get_mount_info(path)
        .is_some_and(|info| info.opts.split(',').any(|have| have == opt))
}

/// `util.mount_is_read_write`.
///
/// Upstream is `get_mount_info(mount_point, get_mnt_opts=True)[-1]
/// .split(",")[0] == "rw"`, which carries two quirks this does not:
/// `[-1]` on a `None` result is a `TypeError` rather than `False`, and on a
/// system old enough to fall back to `/etc/mtab` the `[-1]` lands on the mount
/// point instead of the options, because `parse_mtab` ignores `get_mnt_opts`
/// and returns three fields regardless. Both answer `false` here.
///
/// The `[0]` is upstream's, so this is "the first option is `rw`", not "`rw`
/// appears somewhere" — which holds only because the kernel always writes
/// `rw` or `ro` first.
#[must_use]
pub fn mount_is_read_write(mount_point: &str) -> bool {
    get_mount_info(mount_point)
        .is_some_and(|info| info.opts.split(',').next() == Some("rw"))
}

/// `util.find_devs_with(criteria)` on Linux: `blkid -t<criteria> -odevice`.
///
/// `criteria` is one of `TYPE=<fs>`, `LABEL=<label>` or `UUID=<uuid>`. blkid
/// exits 2 when nothing matches, which upstream accepts alongside 0; a missing
/// `blkid` is an empty list rather than an error, and so is any other failure,
/// because every caller treats "no devices" and "could not look" the same way.
#[must_use]
pub fn find_devs_with(criteria: Option<&str>) -> Vec<PathBuf> {
    if subp::which("blkid").is_none() {
        return Vec::new();
    }

    let mut argv = vec!["blkid".to_owned()];
    if let Some(criteria) = criteria {
        argv.push(format!("-t{criteria}"));
    }
    argv.push("-odevice".to_owned());

    let Ok(output) = Subp::new(&argv).run() else {
        return Vec::new();
    };
    if !matches!(output.code, Some(0 | 2)) {
        return Vec::new();
    }
    output
        .stdout_lossy()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Why [`mount_cb`] could not hand the callback a directory.
#[derive(Debug)]
pub struct MountFailed {
    pub device: PathBuf,
    pub reason: String,
}

impl std::fmt::Display for MountFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed mounting {} due to: {}",
            self.device.display(),
            self.reason
        )
    }
}

/// `util.mount_cb`: mount `device` read-only, run `callback` on the mount
/// point, unmount.
///
/// A device that is already mounted is used where it is and *not* unmounted,
/// exactly as upstream — that is the only reason this needs [`mounts`].
///
/// `mtypes` is upstream's `mtype` list; an empty slice means `["auto"]`, the
/// Linux default. Each is tried in turn and the first that mounts wins.
///
/// `warnings` collects what upstream logs from inside: one line per failed
/// mount attempt, and one if the unmount afterwards fails. A failed unmount is
/// worth surfacing — it leaves a mount point behind that the temporary
/// directory's cleanup cannot remove.
///
/// # Errors
/// [`MountFailed`] when no filesystem type mounted the device.
pub fn mount_cb<T>(
    device: &Path,
    mtypes: &[&str],
    warnings: &mut Vec<String>,
    callback: impl FnOnce(&Path) -> T,
) -> Result<T, MountFailed> {
    let real = std::fs::canonicalize(device).unwrap_or_else(|_| device.to_owned());
    if let Some(existing) = mounts().get(&real) {
        let point = existing.mountpoint.clone();
        return Ok(callback(&point));
    }

    let tmp = crate::path::TempDir::new(std::env::temp_dir(), "cloud-init-mount-")
        .map_err(|error| MountFailed {
            device: device.to_owned(),
            reason: error.to_string(),
        })?;

    let mtypes = if mtypes.is_empty() {
        &["auto"][..]
    } else {
        mtypes
    };
    let mut failure = "no filesystem type was tried".to_owned();
    for mtype in mtypes {
        let argv = [
            "mount",
            "-o",
            "ro",
            "-t",
            mtype,
            &device.to_string_lossy(),
            &tmp.path().to_string_lossy(),
        ];
        match Subp::new(argv).run() {
            Ok(output) if output.success() => {
                let value = callback(tmp.path());
                unmount(tmp.path(), warnings);
                return Ok(value);
            }
            Ok(output) => {
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .clone_into(&mut failure);
            }
            Err(error) => failure = error.to_string(),
        }
        warnings.push(format!(
            "Failed to mount device: '{}' with type: '{mtype}': {failure}",
            device.display()
        ));
    }
    Err(MountFailed {
        device: device.to_owned(),
        reason: failure,
    })
}

fn unmount(point: &Path, warnings: &mut Vec<String>) {
    let reason = match Subp::new(["umount", &point.to_string_lossy()]).run() {
        Ok(output) if output.success() => return,
        Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        Err(error) => error.to_string(),
    };
    warnings.push(format!("Failed to unmount {}: {reason}", point.display()));
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

    /// A `mountinfo` line with the ten columns the parser insists on.
    fn info_line(mount_point: &str, fs_type: &str, devpth: &str, opts: &str) -> String {
        format!("36 35 98:0 /src {mount_point} {opts} - {fs_type} {devpth} rw")
    }

    #[test]
    fn the_deepest_mount_point_covering_the_path_wins() {
        let lines = [
            info_line("/", "ext4", "/dev/sda1", "rw,relatime"),
            info_line("/var", "ext4", "/dev/sdb1", "rw,relatime"),
            info_line("/var/lib/cloud", "btrfs", "/dev/sdc1", "rw,relatime"),
            // Deeper than the path asked about, so ignored.
            info_line("/var/lib/cloud/seed/nocloud", "ext4", "/dev/sdd1", "rw"),
            // Shares no prefix, so ignored.
            info_line("/home", "ext4", "/dev/sde1", "rw"),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();

        let info = parse_mount_info("/var/lib/cloud", &lines)
            .expect("well-formed")
            .expect("a match");
        assert_eq!(info.devpth, "/dev/sdc1");
        assert_eq!(info.fs_type, "btrfs");
        assert_eq!(info.mount_point, "/var/lib/cloud");
        assert_eq!(info.opts, "rw,relatime");
    }

    #[test]
    fn a_later_mount_at_the_same_depth_replaces_an_earlier_one() {
        // The depth guard is `>`, not `>=`, so an overmount of the same
        // directory shadows the one beneath it -- which is the truth.
        let lines = [
            info_line("/mnt", "ext4", "/dev/sda1", "rw"),
            info_line("/mnt", "vfat", "/dev/sdb1", "ro"),
        ];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();

        let info = parse_mount_info("/mnt", &lines)
            .expect("well-formed")
            .expect("a match");
        assert_eq!(info.devpth, "/dev/sdb1");
    }

    #[test]
    fn one_malformed_line_abandons_the_whole_file() {
        // Not "skip the bad line": a partial parse could name a device a
        // caller would then go on to resize.
        let good = info_line("/", "ext4", "/dev/sda1", "rw");
        let lines = vec![good.as_str(), "36 35 98:0 /src /var rw"];

        let err = parse_mount_info("/var", &lines).expect_err("too few columns");
        assert_eq!(
            err.to_string(),
            "Line 2 has two few columns (6): 36 35 98:0 /src /var rw"
        );
    }

    #[test]
    fn a_line_with_no_separator_column_is_rejected() {
        let lines = vec!["36 35 98:0 /src / rw,relatime shared:1 ext4 /dev/sda1 rw"];
        let err = parse_mount_info("/", &lines).expect_err("no '-'");
        assert!(err
            .to_string()
            .starts_with("Did not find column named '-' in line 1:"));
    }

    #[test]
    fn upstream_reports_the_separator_column_where_it_means_the_line_number() {
        // Ten columns, with the '-' at index 8, so there is no device after
        // it. Upstream's loop variable has been overwritten by then, so it
        // says "line 9" for what is line 1. Reproduced deliberately.
        let lines = vec!["36 35 98:0 /src / rw shared:1 master:2 - ext4"];
        let err = parse_mount_info("/", &lines).expect_err("nothing after '-'");
        assert!(err
            .to_string()
            .starts_with("Too few columns after '-' column in line 9:"));
    }

    #[test]
    fn a_path_no_mount_point_covers_is_not_found() {
        let lines = vec!["36 35 98:0 /src /home rw shared:1 - ext4 /dev/sda1 rw"];
        assert!(parse_mount_info("/var", &lines)
            .expect("well-formed")
            .is_none());
    }

    #[test]
    fn an_empty_file_is_not_found_rather_than_an_error() {
        assert!(parse_mount_info("/", &[]).expect("well-formed").is_none());
    }

    #[test]
    fn mtab_matches_the_mount_point_exactly() {
        let text = "\
/dev/sda1 / ext4 rw,relatime 0 0
/dev/sdb1 /var ext4 rw,relatime 0 0
";
        // A prefix is not a match here, unlike parse_mount_info.
        assert!(parse_mtab("/var/lib/cloud", text).is_none());
        let info = parse_mtab("/var", text).expect("a match");
        assert_eq!(info.devpth, "/dev/sdb1");
        assert_eq!(info.fs_type, "ext4");
        assert!(info.opts.is_empty());
    }

    #[test]
    fn a_proc_mounts_line_becomes_an_entry_keyed_by_device() {
        let text = "\
/dev/sda1 /boot ext4 rw,relatime 0 0
/dev/sr0 /media/cd iso9660 ro,nosuid 0 0
";
        let mounted = parse_proc_mounts(text);
        assert_eq!(mounted.len(), 2);
        let cd = &mounted[Path::new("/dev/sr0")];
        assert_eq!(cd.fstype, "iso9660");
        assert_eq!(cd.mountpoint, Path::new("/media/cd"));
        assert_eq!(cd.opts, "ro,nosuid");
    }

    #[test]
    fn a_line_that_is_not_six_fields_is_skipped() {
        let text = "\
proc /proc proc rw 0 0
this line is short
/dev/sda1 /boot ext4 rw 0 0 extra
";
        let mounted = parse_proc_mounts(text);
        assert_eq!(mounted.len(), 1);
        assert!(mounted.contains_key(Path::new("proc")));
    }

    #[test]
    fn octal_escapes_in_a_mount_point_are_undone() {
        let text = "/dev/sda1 /mnt/my\\040disk\\011x ext4 rw 0 0\n";
        let mounted = parse_proc_mounts(text);
        assert_eq!(
            mounted[Path::new("/dev/sda1")].mountpoint,
            Path::new("/mnt/my disk\tx")
        );
    }

    #[test]
    fn a_backslash_that_is_not_an_escape_survives() {
        assert_eq!(unescape_octal("a\\b\\4\\999z"), "a\\b\\4\\999z");
        assert_eq!(unescape_octal("trailing\\"), "trailing\\");
    }

    #[test]
    fn a_device_that_cannot_be_mounted_reports_why_and_warns_once_per_type() {
        let mut warnings = Vec::new();
        let error = mount_cb(
            Path::new("/nonexistent/device"),
            &["iso9660", "udf"],
            &mut warnings,
            |_| (),
        )
        .unwrap_err();
        assert_eq!(error.device, Path::new("/nonexistent/device"));
        assert!(error
            .to_string()
            .starts_with("Failed mounting /nonexistent/device due to: "));
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("with type: 'iso9660'"));
        assert!(warnings[1].contains("with type: 'udf'"));
    }

    #[test]
    fn blkid_with_an_impossible_criteria_finds_nothing() {
        assert!(find_devs_with(Some("LABEL=cloud-init-rs-no-such-label")).is_empty());
    }
}
