//! The `dscheck_*` functions: one per datasource, plus the predicates they
//! share.
//!
//! Each returns [`DsCheck::Found`], [`DsCheck::Maybe`] or
//! [`DsCheck::NotFound`]. A datasource with no check function at all --
//! `MAAS` is in the builtin list but has none -- yields `None`, which the
//! driver turns into the same warning the script emits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ci_sys::subp::{self, Subp};

use crate::config::{check_config, get_value, read_config_key};
use crate::glob;
use crate::read::{is_socket_file, UNAVAILABLE};
use crate::shell::{glob_match, read_line, split_words, trim};
use crate::{DsCheck, Info};

const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const AZURE_CHASSIS: &str = "7783-7084-3265-9085-8269-3286-77";
const EC2_STRICT_ID_DEFAULT: &str = "true";

/// Dispatches `dscheck_<name>`.
pub fn dscheck(info: &mut Info, name: &str) -> Option<DsCheck> {
    let result = match name {
        "Akamai" => akamai(info),
        "AliYun" => aliyun(info),
        "AltCloud" => altcloud(info),
        "Azure" => azure(info),
        "Bigstep" => bigstep(info),
        "CloudCIX" => cloudcix(info),
        "CloudSigma" => cloudsigma(info),
        "CloudStack" => cloudstack(info),
        "ConfigDrive" => configdrive(info),
        "DigitalOcean" => sys_vendor_is(info, "DigitalOcean"),
        "Ec2" => ec2(info),
        "Exoscale" => exoscale(info),
        "GCE" => gce(info),
        "Hetzner" => sys_vendor_is(info, "Hetzner"),
        "IBMCloud" => ibmcloud(info),
        "LXD" => lxd(info),
        "NWCS" => sys_vendor_is(info, "NWCS"),
        "NoCloud" => nocloud(info),
        "None" => DsCheck::NotFound,
        "OVF" => ovf(info),
        "OpenNebula" => opennebula(info),
        "OpenStack" => openstack(info),
        "Oracle" => oracle(info),
        "RbxCloud" => rbxcloud(info),
        "Scaleway" => scaleway(info),
        "SmartOS" => smartos(info),
        "UpCloud" => sys_vendor_is(info, "UpCloud"),
        "VMware" => vmware(info),
        "Vultr" => vultr(info),
        "WSL" => wsl(info),
        _ => return None,
    };
    Some(result)
}

fn found(condition: bool) -> DsCheck {
    if condition {
        DsCheck::Found
    } else {
        DsCheck::NotFound
    }
}

// --- shared predicates ---------------------------------------------------

/// `dmi_*_matches`: false inside a container, where DMI describes the host.
fn dmi_matches(info: &Info, value: &str, pattern: &str) -> bool {
    !info.container && glob_match(pattern, value)
}

fn dmi_product_name_matches(info: &Info, pattern: &str) -> bool {
    dmi_matches(info, &info.dmi_product_name, pattern)
}

fn dmi_product_serial_matches(info: &Info, pattern: &str) -> bool {
    dmi_matches(info, &info.dmi_product_serial, pattern)
}

fn dmi_chassis_asset_tag_matches(info: &Info, pattern: &str) -> bool {
    dmi_matches(info, &info.dmi_chassis_asset_tag, pattern)
}

fn dmi_sys_vendor_is(info: &Info, value: &str) -> bool {
    !info.container && info.dmi_sys_vendor == value
}

fn sys_vendor_is(info: &Info, value: &str) -> DsCheck {
    found(dmi_sys_vendor_is(info, value))
}

fn has_fs_with_label(info: &Info, labels: &[&str]) -> bool {
    let padded = format!(",{},", info.fs.labels);
    labels.iter().any(|l| padded.contains(&format!(",{l},")))
}

fn has_fs_with_uuid(info: &Info, uuid: &str) -> bool {
    format!(",{},", info.fs.uuids).contains(&format!(",{uuid},"))
}

/// `check_seed_dir(name, [files])`, default `meta-data`.
fn check_seed_dir_in(base: &Path, name: &str, files: &[&str]) -> bool {
    let dir = base.join("seed").join(name);
    if !dir.is_dir() {
        return false;
    }
    let files = if files.is_empty() {
        &["meta-data"][..]
    } else {
        files
    };
    files.iter().all(|f| dir.join(f).is_file())
}

fn check_seed_dir(info: &Info, name: &str, files: &[&str]) -> bool {
    check_seed_dir_in(&info.paths.var_lib_cloud, name, files)
}

/// `check_writable_seed_dir`: Ubuntu Core bind-mounts
/// `/writable/system-data/var/lib/cloud` over `/var/lib/cloud`, and the mount
/// may not have happened yet when the generator runs.
fn check_writable_seed_dir(info: &Info, name: &str, files: &[&str]) -> bool {
    let wdir = info.paths.in_root("/writable/system-data");
    if !wdir.is_dir() {
        return false;
    }
    let root = info.paths.root.to_string_lossy().into_owned();
    let vlc = info.paths.var_lib_cloud.to_string_lossy().into_owned();
    let suffix = vlc.strip_prefix(&root).unwrap_or(&vlc);
    let base = PathBuf::from(format!("{}{suffix}", wdir.display()));
    check_seed_dir_in(&base, name, files)
}

/// `is_ds_enabled`.
fn is_ds_enabled(info: &Info, name: &str) -> bool {
    format!(" {} ", info.dslist).contains(&format!(" {name} "))
}

fn block_dev_with_label(info: &Info, label: &str) -> Option<PathBuf> {
    let path = info.paths.dev_disk.join("by-label").join(label);
    is_block_device(&path).then_some(path)
}

fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
}

fn helper(argv: &[&str]) -> Option<subp::Output> {
    subp::which(argv.first().copied().unwrap_or_default())?;
    Subp::new(argv)
        .inherit_env()
        .timeout(Some(HELPER_TIMEOUT))
        .run()
        .ok()
}

/// `| grep "[[:alnum:]]"`: keep the lines with any alphanumeric character, and
/// succeed only if some line survived.
fn grep_alnum(text: &str) -> Option<String> {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| l.chars().any(|c| c.is_ascii_alphanumeric()))
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join("\n"))
    }
}

// --- the simple checks ---------------------------------------------------

fn akamai(info: &Info) -> DsCheck {
    found(dmi_sys_vendor_is(info, "Linode") || dmi_sys_vendor_is(info, "Akamai"))
}

fn cloudstack(info: &Info) -> DsCheck {
    if info.container {
        return DsCheck::NotFound;
    }
    found(dmi_product_name_matches(info, "CloudStack*"))
}

fn cloudcix(info: &Info) -> DsCheck {
    found(dmi_product_name_matches(info, "CloudCIX"))
}

fn exoscale(info: &Info) -> DsCheck {
    found(dmi_product_name_matches(info, "Exoscale*"))
}

fn cloudsigma(info: &Info) -> DsCheck {
    found(dmi_product_name_matches(info, "CloudSigma"))
}

fn gce(info: &Info) -> DsCheck {
    // The product name is not guaranteed (LP: #1674861), hence the serial.
    found(
        dmi_product_name_matches(info, "Google Compute Engine")
            || dmi_product_serial_matches(info, "GoogleCloud-*"),
    )
}

fn oracle(info: &Info) -> DsCheck {
    found(dmi_chassis_asset_tag_matches(info, "OracleCloud.com"))
}

fn opennebula(info: &Info) -> DsCheck {
    found(
        check_seed_dir(info, "opennebula", &[])
            || has_fs_with_label(info, &["CONTEXT", "CDROM"]),
    )
}

fn rbxcloud(info: &Info) -> DsCheck {
    found(has_fs_with_label(info, &["CLOUDMD", "cloudmd"]))
}

fn aliyun(info: &Info) -> DsCheck {
    found(
        check_seed_dir(info, "AliYun", &["meta-data", "user-data"])
            || dmi_product_name_matches(info, "Alibaba Cloud ECS"),
    )
}

fn bigstep(info: &Info) -> DsCheck {
    found(
        info.paths
            .in_var_lib_cloud("/data/seed/bigstep/url")
            .is_file(),
    )
}

fn is_azure_chassis(info: &Info) -> bool {
    dmi_chassis_asset_tag_matches(info, AZURE_CHASSIS)
}

fn azure(info: &Info) -> DsCheck {
    found(is_azure_chassis(info) || check_seed_dir(info, "azure", &["ovf-env.xml"]))
}

fn scaleway(info: &Info) -> DsCheck {
    found(
        info.dmi_sys_vendor == "Scaleway"
            || format!(" {} ", info.kernel_cmdline).contains(" scaleway ")
            || info.paths.in_root("/var/run/scaleway").is_file(),
    )
}

fn vultr(info: &Info) -> DsCheck {
    found(
        dmi_sys_vendor_is(info, "Vultr")
            || format!(" {} ", info.kernel_cmdline).contains(" vultr ")
            || info.paths.in_root("/etc/vultr").is_file(),
    )
}

fn smartos(info: &Info) -> DsCheck {
    // On the container platform uname's version carries the brand, and the
    // socket file distinguishes a real zone from a container inside one.
    let sockfile = info.paths.in_root("/native/.zonecontrol/metadata.sock");
    found(
        dmi_product_name_matches(info, "SmartDC*")
            || (info.uname_kernel_version == "BrandZ virtual linux"
                && sockfile.symlink_metadata().is_ok()),
    )
}

// --- NoCloud, LXD, ConfigDrive -------------------------------------------

fn nocloud(info: &mut Info) -> DsCheck {
    if format!(" {} ", info.dmi_product_serial).contains(" ds=nocloud") {
        return DsCheck::Found;
    }
    for d in ["nocloud", "nocloud-net"] {
        if check_seed_dir(info, d, &["meta-data", "user-data"])
            || check_writable_seed_dir(info, d, &["meta-data", "user-data"])
        {
            return DsCheck::Found;
        }
    }
    if has_fs_with_label(info, &["cidata", "CIDATA"]) {
        return DsCheck::Found;
    }
    // Grep-based and therefore approximate; a NoCloud false positive is
    // cheap, because the datasource itself then finds nothing.
    if check_config(&info.paths, "NoCloud", &[]).is_some() {
        if check_config(&info.paths, "user-data", &[]).is_some()
            && check_config(&info.paths, "meta-data", &[]).is_some()
        {
            return DsCheck::Found;
        }
        if check_config(&info.paths, "seedfrom", &[]).is_some() {
            return DsCheck::Found;
        }
    }
    DsCheck::NotFound
}

fn lxd(info: &mut Info) -> DsCheck {
    if is_socket_file(Path::new("/dev/lxd/sock")) {
        return DsCheck::Found;
    }
    // On LXD KVM instances /dev/lxd/sock does not exist during the generator
    // timeframe, so fall back to the virtio serial port, which works on
    // platforms with no DMI data at all.
    if info.virt != "kvm" && info.virt != "qemu" {
        return DsCheck::NotFound;
    }
    let ports = info.paths.in_root("/sys/class/virtio-ports");
    if !ports.is_dir() {
        return DsCheck::NotFound;
    }
    for entry in glob::expand(&format!("{}/*", ports.display())) {
        let name_file = Path::new(&entry).join("name");
        if !name_file.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&name_file) else {
            info.log
                .warn(&format!("unable to read file: {}", name_file.display()));
            continue;
        };
        let port_name = read_line(&text);
        if port_name == "com.canonical.lxd" || port_name == "org.linuxcontainers.lxd" {
            return DsCheck::Found;
        }
    }
    DsCheck::NotFound
}

fn check_configdrive_v2(info: &mut Info) -> DsCheck {
    let vlc = info.paths.in_var_lib_cloud("/seed/config_drive");
    for d in ["/config-drive".to_owned(), vlc.display().to_string()] {
        let pattern = format!("{d}/openstack/2???-??-??/meta_data.json");
        if glob::expand(&pattern)
            .first()
            .is_some_and(|p| Path::new(p).is_file())
        {
            return DsCheck::Found;
        }
    }
    // At least one cloud (SoftLayer) seeds only 'latest'.
    if vlc.join("openstack/latest/meta_data.json").exists() {
        info.log
            .debug(1, "config drive seeded directory had only 'latest'");
        return DsCheck::Found;
    }

    let ibm_enabled = is_ds_enabled(info, "IBMCloud");
    info.log
        .debug(1, &format!("is_ds_enabled(IBMCloud) = {ibm_enabled}."));
    if ibm_enabled && is_ibm_cloud(info) {
        return DsCheck::NotFound;
    }

    found(has_fs_with_label(info, &["CONFIG-2", "config-2"]))
}

fn configdrive(info: &mut Info) -> DsCheck {
    match check_configdrive_v2(info) {
        DsCheck::Found => DsCheck::Found,
        // check_configdrive_v1 is a stub upstream: it would have to scan every
        // vfat filesystem, so it always reports not-found.
        _ => DsCheck::NotFound,
    }
}

// --- IBM -----------------------------------------------------------------

fn is_ibm_provisioning(info: &mut Info) -> bool {
    let pcfg = info.paths.in_root("/root/provisioningConfiguration.cfg");
    let logf = info.paths.in_root("/root/swinstall.log");
    let mut is_prov = false;
    let mut msg = format!("config '{}' did not exist.", pcfg.display());
    if pcfg.is_file() {
        msg = format!("config '{}' exists.", pcfg.display());
        is_prov = true;
        if logf.is_file() {
            // `-nt`: a log newer than pid 1's environ is from this boot, which
            // means provisioning is still in progress.
            if newer_than(&logf, &info.paths.proc_1_environ) {
                msg = format!("{msg} log '{}' from current boot.", logf.display());
            } else {
                is_prov = false;
                msg = format!("{msg} log '{}' from previous boot.", logf.display());
            }
        } else {
            msg = format!("{msg} log '{}' did not exist.", logf.display());
        }
    }
    info.log
        .debug(2, &format!("ibm_provisioning={is_prov}: {msg}"));
    is_prov
}

fn newer_than(a: &Path, b: &Path) -> bool {
    let Ok(ma) = a.metadata().and_then(|m| m.modified()) else {
        return false;
    };
    match b.metadata().and_then(|m| m.modified()) {
        Ok(mb) => ma > mb,
        // `-nt` is true when the first exists and the second does not.
        Err(_) => true,
    }
}

fn is_ibm_cloud(info: &mut Info) -> bool {
    if let Some(cached) = info.ibm_cloud {
        return cached;
    }
    let mut result = false;
    if info.virt == "xen" {
        result = is_ibm_provisioning(info)
            || has_fs_with_label(info, &["METADATA", "metadata"])
            || (has_fs_with_uuid(info, "9796-932E")
                && has_fs_with_label(info, &["CONFIG-2", "config-2"]));
    }
    info.ibm_cloud = Some(result);
    result
}

fn ibmcloud(info: &mut Info) -> DsCheck {
    if is_ibm_provisioning(info) {
        info.log
            .debug(1, "cloud-init disabled during provisioning on IBMCloud");
        return DsCheck::NotFound;
    }
    found(is_ibm_cloud(info))
}

// --- OpenStack -----------------------------------------------------------

fn openstack(info: &mut Info) -> DsCheck {
    // If a config drive is present the metadata service is not consulted.
    if check_configdrive_v2(info) == DsCheck::Found {
        return DsCheck::NotFound;
    }
    let nova = "OpenStack Nova";
    let compute = "OpenStack Compute";
    if dmi_product_name_matches(info, nova)
        // RDO installs nova and reports Compute (LP: #1675349).
        || dmi_product_name_matches(info, compute)
        || info.pid1_product_name == nova
        || dmi_chassis_asset_tag_matches(info, "OpenTelekomCloud")
        || dmi_chassis_asset_tag_matches(info, "SAP CCloud VM")
        || dmi_chassis_asset_tag_matches(info, "HUAWEICLOUD")
        || dmi_chassis_asset_tag_matches(info, "Samsung Cloud Platform")
        // LP: #1669875: identification by asset tag.
        || dmi_chassis_asset_tag_matches(info, nova)
        || dmi_chassis_asset_tag_matches(info, compute)
    {
        return DsCheck::Found;
    }
    // LP: #1715241: architectures other than x86 are not identified properly,
    // so they get the benefit of the doubt.
    if !glob_match("i?86", &info.uname_machine) && info.uname_machine != "x86_64" {
        return DsCheck::Maybe;
    }
    DsCheck::NotFound
}

// --- Ec2 -----------------------------------------------------------------

fn ec2_read_strict_setting(info: &mut Info, default: &str) -> String {
    let key = "ci.datasource.ec2.strict_id";

    // 4. kernel command line (undocumented).
    let padded = format!(" {} ", info.kernel_cmdline);
    if padded.contains(&format!(" {key}=")) {
        let val = match info.kernel_cmdline.rsplit_once(&format!("{key}=")) {
            Some((_, rest)) => rest,
            None => "",
        };
        let val = val.split(' ').next().unwrap_or_default();
        return if val.is_empty() {
            default.to_owned()
        } else {
            val.to_owned()
        };
    }

    // 3. system config, but only cloud.cfg and a case-insensitively named
    // *Ec2*.cfg drop-in.
    let globs = vec![
        info.paths.etc_ci_cfg.to_string_lossy().into_owned(),
        format!("{}/*[Ee][Cc]2*.cfg", info.paths.etc_ci_cfg_d.display()),
    ];
    if let Some(found) = check_config(&info.paths, "strict_id", &globs) {
        info.log.debug(
            2,
            &format!("{} set strict_id to {}", found.fname, found.value),
        );
        return found.value;
    }

    // 2. ds-identify config.
    if info.paths.di_config.is_file() {
        if let Ok(text) = std::fs::read_to_string(&info.paths.di_config) {
            let value = read_config_key(&text, key).unwrap_or_default();
            return if value.is_empty() {
                default.to_owned()
            } else {
                value
            };
        }
    }

    // 1. Builtin default.
    default.to_owned()
}

fn ec2_identify_platform(info: &Info, default: &str) -> String {
    if glob_match("*.brightbox.com", &info.dmi_product_serial) {
        return "Brightbox".to_owned();
    }
    if glob_match("*.zstack.io", &info.dmi_chassis_asset_tag) {
        return "ZStack".to_owned();
    }
    match info.dmi_sys_vendor.as_str() {
        "e24cloud" => return "E24cloud".to_owned(),
        "Tilaa" => return "Tilaa".to_owned(),
        _ => {}
    }
    if info.dmi_product_name == "3DS Outscale VM"
        && info.dmi_sys_vendor == "3DS Outscale"
    {
        return "Outscale".to_owned();
    }

    // The xen-specific /sys/hypervisor/uuid starts with "ec2" on AWS.
    let hvuuid = info.paths.sys_hypervisor.join("uuid");
    if let Ok(text) = std::fs::read_to_string(&hvuuid) {
        if read_line(&text).starts_with("ec2") {
            return "AWS".to_owned();
        }
    }

    // Otherwise the first octet of the product UUID, in either byte order:
    // EC2E1916-... or 45E12AEC-...
    let start_uuid = match info.dmi_product_uuid.split_once('-') {
        Some((head, _)) => head,
        None => &info.dmi_product_uuid,
    };
    if glob_match("[Ee][Cc]2*", start_uuid)
        || glob_match("*2[0-9a-fA-F][Ee][Cc]", start_uuid)
    {
        return "AWS".to_owned();
    }
    default.to_owned()
}

fn ec2(info: &mut Info) -> DsCheck {
    if check_seed_dir(info, "ec2", &["meta-data", "user-data"]) {
        return DsCheck::Found;
    }
    if info.container {
        return DsCheck::NotFound;
    }

    let unknown = "Unknown";
    let platform = ec2_identify_platform(info, unknown);
    info.log.debug(1, &format!("ec2 platform is '{platform}'."));
    if platform != unknown {
        return DsCheck::Found;
    }

    let default = EC2_STRICT_ID_DEFAULT;
    let mut strict = ec2_read_strict_setting(info, default);
    if !matches!(strict.as_str(), "true" | "false" | "warn")
        && !glob_match("warn,[0-9]*", &strict)
    {
        info.log.warn(&format!(
            "datasource/Ec2/strict_id was set to invalid '{strict}'. using '{default}'"
        ));
        default.clone_into(&mut strict);
    }

    info.excfg = format!("datasource: {{Ec2: {{strict_id: \"{strict}\"}}}}");
    if strict == "true" {
        DsCheck::NotFound
    } else {
        DsCheck::Maybe
    }
}

// --- AltCloud ------------------------------------------------------------

fn probe_floppy(info: &mut Info) -> bool {
    if let Some(cached) = info.floppy_probed {
        return cached;
    }
    let fpath = Path::new("/dev/floppy");
    let result = is_block_device(fpath)
        // Busybox modprobe has no long options, hence -b.
        && helper(&["modprobe", "-b", "floppy"]).is_some_and(|o| o.success())
        && (subp::which("udevadm").is_none()
            || helper(&["udevadm", "settle", "--exit-if-exists=/dev/floppy"])
                .is_some_and(|o| o.success()))
        && is_block_device(fpath);
    info.floppy_probed = Some(result);
    result
}

fn altcloud(info: &mut Info) -> DsCheck {
    let cinfo = info.paths.in_root("/etc/sysconfig/cloud-info");
    let ctype = match std::fs::read_to_string(&cinfo) {
        Ok(text) if cinfo.is_file() => read_line(&text),
        _ => info.dmi_product_name.clone(),
    };
    if glob_match("[Rr][Hh][Ee][Vv]", &ctype) {
        if !probe_floppy(info) {
            return DsCheck::NotFound;
        }
    } else if glob_match("[Vv][Ss][Pp][Hh][Ee][Rr][Ee]", &ctype) {
        if block_dev_with_label(info, "CDROM").is_none() {
            return DsCheck::NotFound;
        }
    } else {
        return DsCheck::NotFound;
    }
    // Upstream leaves the actual user-data.txt check unimplemented, so the
    // best it can say is "maybe".
    DsCheck::Maybe
}

// --- OVF and VMware ------------------------------------------------------

fn vmware_guestinfo(key: &str, with_stderr: bool) -> Option<String> {
    let arg = format!("info-get guestinfo.{key}");
    for argv in [
        vec!["vmware-rpctool", arg.as_str()],
        vec!["vmtoolsd", "--cmd", arg.as_str()],
    ] {
        let Some(out) = helper(&argv) else {
            continue;
        };
        let mut text = out.stdout_lossy().into_owned();
        if with_stderr {
            text.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        if let Some(kept) = grep_alnum(&text) {
            return Some(kept);
        }
    }
    None
}

fn vmware_has_tool() -> bool {
    subp::which("vmware-rpctool").is_some() || subp::which("vmtoolsd").is_some()
}

fn ovf_vmware_transport_guestinfo(info: &mut Info) -> bool {
    if info.virt != "vmware" || !vmware_has_tool() {
        return false;
    }
    let Some(out) = vmware_guestinfo("ovfEnv", true) else {
        info.log
            .debug(1, "Running on vmware but query returned 1: ");
        return false;
    };
    if !(out.starts_with("<?xml") || out.starts_with("<?XML")) {
        info.log
            .debug(1, &format!("guestinfo.ovfEnv had non-xml content: {out}"));
        return false;
    }
    info.log.debug(1, "Found guestinfo transport.");
    true
}

fn is_cdrom_ovf(info: &mut Info, dev: &str, label: &str) -> bool {
    // Only real optical devices; anything else with an iso9660 filesystem is
    // some other transport.
    if !(glob_match("/dev/sr[0-9]", dev) || glob_match("/dev/hd[a-z]", dev)) {
        info.log.debug(1, &format!("skipping iso dev {dev}"));
        return false;
    }
    info.log.debug(1, &format!("got label={label}"));
    if matches!(
        label,
        "OVF-TRANSPORT" | "ovf-transport" | "OVFENV" | "ovfenv" | "OVF ENV" | "ovf env"
    ) {
        return true;
    }
    // rd_rdfe is Azure.
    if matches!(label, "config-2" | "CONFIG-2" | "cidata" | "CIDATA")
        || glob_match("rd_rdfe_stable*", label)
    {
        return false;
    }

    let basename = dev.rsplit('/').next().unwrap_or(dev);
    let sfile = info.paths.sys_class_block.join(basename).join("size");
    if !sfile.is_file() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&sfile) else {
        info.log
            .warn(&format!("failed reading from {}", sfile.display()));
        return false;
    };
    let size: u64 = read_line(&text).parse().unwrap_or(0);
    // Units are 512-byte sectors; anything 10MB or larger is not an OVF
    // transport disc. The truncating division is upstream's.
    #[allow(clippy::integer_division)]
    let megabytes = size / 2048;
    if megabytes >= 10 {
        info.log.debug(
            2,
            &format!("{dev}: size {megabytes}MB is considered too large for OVF"),
        );
        return false;
    }

    let target = info.paths.in_root(dev);
    let Ok(bytes) = std::fs::read(&target) else {
        return false;
    };
    let needle = "http://schemas.dmtf.org/ovf/environment/1";
    String::from_utf8_lossy(&bytes)
        .to_lowercase()
        .contains(&needle.to_lowercase())
}

fn has_ovf_cdrom(info: &mut Info) -> bool {
    let devs = info.fs.iso9660_devs.clone();
    if devs.starts_with(&format!("{UNAVAILABLE}:")) {
        return false;
    }
    for token in devs.split(',').filter(|t| !t.is_empty()) {
        let dev = token.split(':').next().unwrap_or(token);
        let (dev, label) = match dev.split_once('=') {
            Some((dev, label)) => (dev, label),
            None => (dev, dev),
        };
        if is_cdrom_ovf(info, dev, label) {
            return true;
        }
    }
    false
}

fn ovf(info: &mut Info) -> DsCheck {
    if check_seed_dir(info, "ovf", &["ovf-env.xml"]) {
        return DsCheck::Found;
    }
    if info.virt == "none" {
        return DsCheck::NotFound;
    }
    // Azure also provides OVF; let the Azure check claim it.
    if is_azure_chassis(info) {
        return DsCheck::NotFound;
    }
    if ovf_vmware_transport_guestinfo(info) {
        return DsCheck::Found;
    }
    found(has_ovf_cdrom(info))
}

fn vmware_guest_customization(info: &mut Info) -> bool {
    if info.virt != "vmware" {
        return false;
    }
    let pre = info.paths.in_root("/usr/lib");
    let ppath = "plugins/vmsvc/libdeployPkgPlugin.so";
    let mut found_pkg = false;
    for pkg in ["vmware-tools", "open-vm-tools"] {
        let candidates = [
            format!("{}/{pkg}/{ppath}", pre.display()),
            format!("{}64/{pkg}/{ppath}", pre.display()),
            format!("{}/x86_64-linux-gnu/{pkg}/{ppath}", pre.display()),
            format!("{}/aarch64-linux-gnu/{pkg}/{ppath}", pre.display()),
            format!("{}/i386-linux-gnu/{pkg}/{ppath}", pre.display()),
        ];
        if candidates.iter().any(|c| Path::new(c).is_file()) {
            found_pkg = true;
            break;
        }
    }
    if !found_pkg {
        return false;
    }
    // Customization is off unless disable_vmware_customization is explicitly
    // false.
    let key = "disable_vmware_customization";
    let Some(matched) = check_config(&info.paths, key, &[]) else {
        return false;
    };
    let Some(value) = get_value(&mut info.log, key, &matched.value) else {
        return false;
    };
    info.log
        .debug(2, &format!("{} set {key} to {value}", matched.fname));
    matches!(value.as_str(), "0" | "false" | "False")
}

fn vmware(info: &mut Info) -> DsCheck {
    // Transports are checked in the same order as DataSourceVMware._get_data.
    let has_envvar = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty());
    if has_envvar("VMX_GUESTINFO")
        && (has_envvar("VMX_GUESTINFO_METADATA")
            || has_envvar("VMX_GUESTINFO_USERDATA")
            || has_envvar("VMX_GUESTINFO_VENDORDATA"))
    {
        return DsCheck::Found;
    }
    if info.virt != "vmware" || !vmware_has_tool() {
        return DsCheck::NotFound;
    }
    if vmware_guestinfo("metadata", false).is_some()
        || vmware_guestinfo("userdata", false).is_some()
        || vmware_guestinfo("vendordata", false).is_some()
    {
        return DsCheck::Found;
    }
    found(vmware_guest_customization(info))
}

// --- WSL -----------------------------------------------------------------

fn wsl_path(params: &str, path: &str) -> Option<String> {
    let out = helper(&["wslpath", params, path])?;
    out.success().then(|| out.stdout_trimmed())
}

fn wsl_profile_dir(mountpoints: &str) -> Option<String> {
    for m in split_words(mountpoints) {
        let cmdexe = format!("{m}/Windows/System32/cmd.exe");
        if subp::which(&cmdexe).is_none() {
            continue;
        }
        // WSL's own /init starts the Windows shell, which prints %USERPROFILE%.
        let out = helper(&["/init", &cmdexe, "/c", "echo %USERPROFILE%"])?;
        let stdout = out.stdout_lossy().into_owned();
        let profiledir = stdout.trim_end_matches(char::is_control);
        if profiledir.is_empty() {
            continue;
        }
        // wslpath translates between Windows and Linux paths, honouring where
        // the drives are actually mounted.
        return wsl_path("-au", profiledir);
    }
    None
}

fn wsl_instance_name() -> String {
    // "//wsl.localhost/Ubuntu/" -> "Ubuntu"
    let Some(path) = wsl_path("-am", "/") else {
        return String::new();
    };
    let rest = match path.strip_prefix("//") {
        Some(rest) => match rest.split_once('/') {
            Some((_, tail)) => tail,
            None => rest,
        },
        None => &path,
    };
    rest.strip_suffix('/').unwrap_or(rest).to_owned()
}

fn os_release(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = trim(line);
        let (k, v) = line.split_once('=')?;
        if k == key {
            return Some(crate::shell::unquote(v).to_owned());
        }
    }
    None
}

fn wsl(info: &mut Info) -> DsCheck {
    if info.uname_kernel_name != "Linux" || info.virt != "wsl" {
        return DsCheck::NotFound;
    }

    // The Windows filesystem is exposed as one 9p drvfs mount per drive; with
    // none of them the datasource has nowhere to read from.
    let mounts =
        std::fs::read_to_string(info.paths.in_root("/proc/mounts")).unwrap_or_default();
    let mountpoints: Vec<&str> = mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let is_drvfs = fields.get(2) == Some(&"9p")
                && fields.get(3).is_some_and(|o| o.contains("aname=drvfs;"));
            is_drvfs.then(|| fields.get(1).copied()).flatten()
        })
        .collect();
    if mountpoints.is_empty() {
        info.log.debug(
            1,
            "WSL datasource requires access to Windows drives mount points",
        );
        return DsCheck::NotFound;
    }

    let Some(profile_dir) = wsl_profile_dir(&mountpoints.join(" ")) else {
        info.log.debug(1, "%USERPROFILE% directory not found");
        return DsCheck::NotFound;
    };

    let base = Path::new(&profile_dir);
    if !base.join(".cloud-init").is_dir()
        && !base.join(".ubuntupro/.cloud-init").is_dir()
    {
        info.log.debug(
            1,
            &format!("No .cloud-init directories found in {profile_dir}"),
        );
        return DsCheck::NotFound;
    }

    let instance_name = wsl_instance_name();
    let osr = info.paths.in_root("/etc/os-release");
    let name = os_release(&osr, "NAME").unwrap_or_default();
    let id = os_release(&osr, "ID").unwrap_or_else(|| "linux".to_owned());
    let version = os_release(&osr, "VERSION_ID")
        .or_else(|| os_release(&osr, "VERSION_CODENAME"))
        .unwrap_or_default();

    // Ubuntu Pro configuration takes precedence, but only on Ubuntu.
    if name == "Ubuntu" {
        let dir = base.join(".ubuntupro/.cloud-init");
        for file in [
            format!("{instance_name}.user-data"),
            "agent.yaml".to_owned(),
        ] {
            let candidate = dir.join(&file);
            if candidate.is_file() {
                info.log.debug(
                    1,
                    &format!(
                        "Found applicable pro data file for this instance at: {}",
                        candidate.display()
                    ),
                );
                return DsCheck::Found;
            }
        }
    }

    let dir = base.join(".cloud-init");
    for file in [
        format!("{instance_name}.user-data"),
        format!("{id}-{version}.user-data"),
        format!("{id}-all.user-data"),
        "default.user-data".to_owned(),
    ] {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            info.log.debug(
                1,
                &format!(
                    "Found applicable user data file for this instance at: {}",
                    candidate.display()
                ),
            );
            return DsCheck::Found;
        }
    }

    info.log.debug(
        1,
        &format!(
            "Didn't find any applicable user data file for instance named {instance_name} in {}",
            dir.display()
        ),
    );
    DsCheck::NotFound
}
