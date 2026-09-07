//! The `read_*` collectors: DMI, virtualisation, filesystems, command line.
//!
//! Everything here is a straight transliteration, including the parts that
//! look wrong. Where a quirk is load-bearing it is called out in a comment; the
//! differential harness pins the rest.

use std::path::Path;
use std::time::Duration;

use ci_sys::subp::{self, Subp};

use crate::log::Log;
use crate::paths::Paths;
use crate::shell::{read_line, split_words};

pub const UNAVAILABLE: &str = "unavailable";
pub const ERROR: &str = "error";

/// A short leash: these helpers run in the boot path.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

fn helper(argv: &[&str]) -> Option<subp::Output> {
    Subp::new(argv)
        .inherit_env()
        .timeout(Some(HELPER_TIMEOUT))
        .run()
        .ok()
}

/// `ensure_sane_path`: the unit file's `PATH` may be minimal, and `blkid` and
/// `dmidecode` live in `/sbin`.
pub fn ensure_sane_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut entries: Vec<String> = if current.is_empty() {
        Vec::new()
    } else {
        current.split(':').map(str::to_owned).collect()
    };
    for wanted in ["/sbin", "/usr/sbin", "/bin", "/usr/bin"] {
        if entries
            .iter()
            .any(|e| e == wanted || e == &format!("{wanted}/"))
        {
            continue;
        }
        entries.push(wanted.to_owned());
    }
    std::env::set_var("PATH", entries.join(":"));
}

/// `read_uname_info`: (kernel name, kernel version, machine).
#[must_use]
pub fn read_uname_info(log: &mut Log) -> (String, String, String) {
    let Some(out) = helper(&["uname", "-svm"]) else {
        log.error("failed reading uname with 'uname -svm'");
        return (String::new(), String::new(), String::new());
    };
    if !out.success() {
        log.error("failed reading uname with 'uname -svm'");
        return (String::new(), String::new(), String::new());
    }
    parse_uname(&out.stdout_trimmed())
}

fn parse_uname(out: &str) -> (String, String, String) {
    let words = split_words(out);
    let name = words.first().copied().unwrap_or_default().to_owned();
    // Everything between the first and last word is the kernel version, which
    // is the one field known to contain spaces.
    let machine = if words.len() > 1 {
        words.last().copied().unwrap_or_default().to_owned()
    } else {
        String::new()
    };
    let version = if words.len() > 2 {
        words.get(1..words.len() - 1).unwrap_or_default().join(" ")
    } else {
        String::new()
    };
    (name, version, machine)
}

/// `get_dmi_field`.
///
/// When `/sys/class/dmi/id` exists but the requested attribute does not, the
/// script deliberately does *not* fall back to `dmidecode`; a kernel that
/// exposes the directory has already decided the field is absent.
#[must_use]
pub fn get_dmi_field(log: &mut Log, paths: &Paths, field: &str) -> String {
    let dir = &paths.sys_class_dmi_id;
    if dir.is_dir() {
        let path = dir.join(field);
        if path.is_file() {
            return match std::fs::read_to_string(&path) {
                Ok(text) => read_line(&text),
                Err(_) => ERROR.to_owned(),
            };
        }
        return UNAVAILABLE.to_owned();
    }
    dmi_decode(log, field).unwrap_or_else(|| ERROR.to_owned())
}

fn dmi_decode(log: &mut Log, sys_field: &str) -> Option<String> {
    if subp::which("dmidecode").is_none() {
        log.warn(&format!("No dmidecode program. Cannot read {sys_field}."));
        return None;
    }
    let dmi_field = match sys_field {
        "sys_vendor" => "system-manufacturer",
        "product_name" => "system-product-name",
        "product_uuid" => "system-uuid",
        "product_serial" => "system-serial-number",
        "chassis_asset_tag" => "chassis-asset-tag",
        _ => {
            log.error(&format!(
                "Unknown field {sys_field}. Cannot call dmidecode."
            ));
            return None;
        }
    };
    let out = helper(&["dmidecode", "--quiet", &format!("--string={dmi_field}")])?;
    if !out.success() {
        return None;
    }
    Some(out.stdout_trimmed())
}

/// `detect_virt`.
#[must_use]
pub fn detect_virt(log: &mut Log, paths: &Paths, kernel_name: &str) -> String {
    let mut virt = UNAVAILABLE.to_owned();
    if paths.in_root("/run/systemd").is_dir() {
        let systemd_virt = std::env::var("SYSTEMD_VIRTUALIZATION").unwrap_or_default();
        if systemd_virt.is_empty() {
            if let Some(out) = helper(&["systemd-detect-virt"]) {
                let mut text = out.stdout_lossy().into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                let text = text.trim_end_matches('\n').to_owned();
                // systemd < 251 exits non-zero for "none".
                if out.success() || text == "none" {
                    virt = text;
                }
            }
            log.debug(2, &format!("detected {virt} via ds-identify"));
        } else {
            // `VIRTUALIZATION=container-other:lxc` style; keep the tail.
            virt = match systemd_virt.split_once(':') {
                Some((_, tail)) => tail.to_owned(),
                None => systemd_virt,
            };
            log.debug(
                2,
                &format!("detected {virt} via env variable SYSTEMD_VIRTUALIZATION"),
            );
        }
    } else if subp::which("virt-what").is_some() {
        if let Some(out) = helper(&["virt-what"]) {
            let mut text = out.stdout_lossy().into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            // `virt-what 2>&1 | head -n 1`: the pipeline's status is head's, so
            // a failing virt-what still sets the value.
            let first = text.lines().next().unwrap_or_default();
            virt = match first {
                "ibm_systemz-zvm" => "zvm".to_owned(),
                "hyperv" => "microsoft".to_owned(),
                "virtualbox" => "oracle".to_owned(),
                "xen-domU" => "xen".to_owned(),
                other => other.to_owned(),
            };
        }
    } else if kernel_name == "FreeBSD" || kernel_name == "Dragonfly" {
        // Out of scope; see COMPAT.md.
    }
    virt
}

/// `is_container`.
#[must_use]
pub fn is_container(virt: &str) -> bool {
    matches!(
        virt,
        "container-other"
            | "lxc"
            | "lxc-libvirt"
            | "systemd-nspawn"
            | "docker"
            | "rkt"
            | "jail"
    )
}

/// `read_kernel_cmdline`.
#[must_use]
pub fn read_kernel_cmdline(paths: &Paths, container: bool) -> String {
    if container {
        let p1 = &paths.proc_1_cmdline;
        if p1.is_file() {
            if let Ok(bytes) = std::fs::read(p1) {
                let text: String = bytes
                    .iter()
                    .map(|&b| if b == 0 { ' ' } else { b as char })
                    .collect();
                return text;
            }
        }
        return format!("{UNAVAILABLE}:container");
    }
    if paths.proc_cmdline.is_file() {
        if let Ok(text) = std::fs::read_to_string(&paths.proc_cmdline) {
            return read_line(&text);
        }
        return String::new();
    }
    format!("{UNAVAILABLE}:no-cmdline")
}

/// `read_pid1_product_name`.
///
/// An unreadable `/proc/1/environ` leaves the variable *empty*, not
/// "unavailable": the script returns before the assignment.
#[must_use]
pub fn read_pid1_product_name(paths: &Paths) -> String {
    let Ok(bytes) = std::fs::read(&paths.proc_1_environ) else {
        return String::new();
    };
    for token in bytes.split(|&b| b == 0) {
        let token = String::from_utf8_lossy(token);
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if key == "product_name" {
            return value.to_owned();
        }
    }
    UNAVAILABLE.to_owned()
}

/// What `read_fs_info` fills in.
#[derive(Debug, Clone, Default)]
pub struct FsInfo {
    pub labels: String,
    pub uuids: String,
    pub iso9660_devs: String,
}

/// `read_fs_info_linux`.
#[must_use]
pub fn read_fs_info(log: &mut Log, container: bool) -> FsInfo {
    if container {
        // blkid tells you nothing useful inside a container.
        return FsInfo {
            labels: format!("{UNAVAILABLE}:container"),
            uuids: String::new(),
            iso9660_devs: format!("{UNAVAILABLE}:container"),
        };
    }
    let out = helper(&["blkid", "-c", "/dev/null", "-o", "export"]);
    let text = match &out {
        Some(o) if o.success() => o.stdout_lossy().into_owned(),
        other => {
            let code = other.as_ref().and_then(|o| o.code).unwrap_or(1);
            log.error(&format!(
                "failed running [{code}]: blkid -c /dev/null -o export"
            ));
            return FsInfo {
                labels: format!("{UNAVAILABLE}:{ERROR}"),
                uuids: format!("{UNAVAILABLE}:{ERROR}"),
                iso9660_devs: format!("{UNAVAILABLE}:{ERROR}"),
            };
        }
    };
    parse_blkid_export(&text)
}

fn parse_blkid_export(text: &str) -> FsInfo {
    let (mut labels, mut uuids, mut isodevs) =
        (String::new(), String::new(), String::new());
    let (mut dev, mut label, mut ftype) = (String::new(), String::new(), String::new());
    let flush = |dev: &str, label: &str, ftype: &str, isodevs: &mut String| {
        if !dev.is_empty() && ftype == "iso9660" {
            isodevs.push(',');
            isodevs.push_str(dev);
            isodevs.push('=');
            isodevs.push_str(label);
        }
    };
    for line in text.lines().filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("DEVNAME=") {
            flush(&dev, &label, &ftype, &mut isodevs);
            ftype.clear();
            label.clear();
            rest.clone_into(&mut dev);
        } else if line.starts_with("LABEL=") || line.starts_with("LABEL_FATBOOT=") {
            label.clear();
            if let Some((_, value)) = line.split_once('=') {
                label.push_str(value);
            }
            labels.push_str(&label);
            labels.push(',');
        } else if let Some(rest) = line.strip_prefix("TYPE=") {
            ftype.clear();
            ftype.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("UUID=") {
            uuids.push_str(rest);
            uuids.push(',');
        }
    }
    flush(&dev, &label, &ftype, &mut isodevs);
    FsInfo {
        labels: labels.trim_end_matches(',').to_owned(),
        uuids: uuids.trim_end_matches(',').to_owned(),
        iso9660_devs: isodevs.strip_prefix(',').unwrap_or(&isodevs).to_owned(),
    }
}

/// `read_uptime`: the first field of `/proc/uptime`, or `unavailable`.
#[must_use]
pub fn read_uptime(paths: &Paths) -> String {
    if !paths.proc_uptime.is_file() {
        return UNAVAILABLE.to_owned();
    }
    let Ok(text) = std::fs::read_to_string(&paths.proc_uptime) else {
        return UNAVAILABLE.to_owned();
    };
    // `read up _`: an empty file still succeeds at open but read returns
    // non-zero, so _RET keeps the unavailable marker.
    split_words(&text)
        .first()
        .copied()
        .unwrap_or(UNAVAILABLE)
        .to_owned()
}

/// `is_socket_file`.
#[must_use]
pub fn is_socket_file(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
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
    fn uname_output_splits_first_rest_last() {
        let (name, version, machine) =
            parse_uname("Linux #1 SMP PREEMPT_DYNAMIC Debian 6.1.0 x86_64");
        assert_eq!(name, "Linux");
        assert_eq!(version, "#1 SMP PREEMPT_DYNAMIC Debian 6.1.0");
        assert_eq!(machine, "x86_64");
    }

    #[test]
    fn uname_with_two_words_has_no_version() {
        let (name, version, machine) = parse_uname("Linux x86_64");
        assert_eq!(name, "Linux");
        assert_eq!(version, "");
        assert_eq!(machine, "x86_64");
    }

    #[test]
    fn blkid_export_accumulates_labels_uuids_and_iso_devices() {
        let text = "\
DEVNAME=/dev/sda1
LABEL=cloudimg-rootfs
UUID=11111111-1111-1111-1111-111111111111
TYPE=ext4

DEVNAME=/dev/sr0
LABEL=cidata
UUID=2025-01-01-00-00-00-00
TYPE=iso9660

DEVNAME=/dev/sda15
LABEL_FATBOOT=UEFI
TYPE=vfat
";
        let info = parse_blkid_export(text);
        assert_eq!(info.labels, "cloudimg-rootfs,cidata,UEFI");
        assert_eq!(
            info.uuids,
            "11111111-1111-1111-1111-111111111111,2025-01-01-00-00-00-00"
        );
        assert_eq!(info.iso9660_devs, "/dev/sr0=cidata");
    }

    #[test]
    fn the_last_device_is_flushed_too() {
        let text = "DEVNAME=/dev/sr0\nLABEL=config-2\nTYPE=iso9660\n";
        assert_eq!(parse_blkid_export(text).iso9660_devs, "/dev/sr0=config-2");
    }
}
