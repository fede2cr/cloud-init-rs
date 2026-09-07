//! `net.cmdline`: network config an initramfs or the kernel command line left
//! behind.
//!
//! Two unrelated sources share the upstream module: the `network-config=`
//! kernel parameter, and the klibc `/run/net-*.conf` files a netbooted or
//! iSCSI-booted Debian initramfs writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ci_config::{Object, Value};

/// The `network-config=` value that means "leave the network alone".
const DISABLED: &str = "disabled";

/// `_OPEN_ISCSI_INTERFACE_FILE`, relative to the run directory.
const OPEN_ISCSI_INTERFACE: &str = "initramfs/open-iscsi.interface";

/// `_decomp_gzip`: gunzip, or hand back what came in.
fn decomp_gzip(blob: Vec<u8>) -> Vec<u8> {
    ci_core::gzip::decompress(&blob).unwrap_or(blob)
}

/// `_b64dgz`: base64-decode, then gunzip if it turns out to be gzipped.
///
/// An undecodable value is an empty string upstream, not an error.
fn b64dgz(data: &str, log: &mut ci_log::Logger) -> Vec<u8> {
    let Some(blob) = ci_core::b64::decode(data) else {
        log.error(
            "cmdline.py",
            &format!(
                "Expected base64 encoded kernel command line parameter \
                 network-config. Ignoring network-config={data}."
            ),
        );
        return Vec::new();
    };
    decomp_gzip(blob)
}

/// `read_kernel_cmdline_config`.
///
/// The last `network-config=` on the line wins even when it is empty, which is
/// upstream's loop-then-test order: a trailing empty one disables an earlier
/// payload rather than losing to it.
#[must_use]
pub fn read_kernel_cmdline_config(
    cmdline: &str,
    log: &mut ci_log::Logger,
) -> Option<Object> {
    if !cmdline.contains("network-config=") {
        return None;
    }
    let data64 = cmdline
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("network-config="))
        .next_back()
        .filter(|value| !value.is_empty())?;
    if data64 == DISABLED {
        let mut cfg = Object::new();
        cfg.insert("config".to_owned(), Value::from(DISABLED));
        return Some(cfg);
    }

    let blob = b64dgz(data64, log);
    ci_config::yaml::load_yaml(
        &String::from_utf8_lossy(&blob),
        ci_config::Limits::default(),
    )
    .ok()
    .and_then(|value| match value {
        Value::Object(object) => Some(object),
        _ => None,
    })
}

/// `net.is_disabled_cfg`.
#[must_use]
pub fn is_disabled_cfg(cfg: Option<&Object>) -> bool {
    cfg.and_then(|cfg| cfg.get("config"))
        .and_then(Value::as_str)
        == Some(DISABLED)
}

/// The `ValueError`s the klibc reader lets escape, with upstream's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KlibcError {
    NoDevice,
    UnexpectedProto(String),
    MultipleMacs {
        name: String,
        files: String,
        old: String,
        new: String,
    },
    Shell(ci_core::shlex::Error),
}

impl std::fmt::Display for KlibcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no 'DEVICE' or 'DEVICE6' entry in data"),
            Self::UnexpectedProto(proto) => {
                write!(f, "Unexpected value for PROTO: {proto}")
            }
            Self::MultipleMacs {
                name,
                files,
                old,
                new,
            } => write!(
                f,
                "device '{name}' was defined multiple times ({files}) but had \
                 differing mac addresses: {old} -> {new}."
            ),
            Self::Shell(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for KlibcError {}

/// `str(None)` in the one message that interpolates a missing mac.
fn py_str(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), str::to_owned)
}

/// `_klibc_to_config_entry`: one `/run/net-*.conf` file to a v1 `physical`.
///
/// # Errors
///
/// A file with no device name or an unusable `PROTO`, both `ValueError`
/// upstream.
pub fn klibc_to_config_entry(
    content: &str,
    mac_addrs: &BTreeMap<String, String>,
) -> Result<(String, Object), KlibcError> {
    let data =
        ci_core::shlex::load_shell_content(content).map_err(KlibcError::Shell)?;
    let get = |key: &str| data.get(key).map(String::as_str);

    let name = get("DEVICE")
        .or_else(|| get("DEVICE6"))
        .ok_or(KlibcError::NoDevice)?
        .to_owned();

    // ipconfig on precise does not write PROTO, and the v6 half writes
    // IPV6PROTO instead. A file that only names a boot file is a dhcp lease.
    let mut proto = get("PROTO").or_else(|| get("IPV6PROTO")).unwrap_or(
        if get("filename").is_some() {
            "dhcp"
        } else {
            "none"
        },
    );
    if proto == "static" || proto == "off" {
        proto = "none";
    }
    if !matches!(proto, "none" | "dhcp" | "dhcp6") {
        return Err(KlibcError::UnexpectedProto(proto.to_owned()));
    }

    let mut iface = Object::new();
    iface.insert("type".to_owned(), Value::from("physical"));
    iface.insert("name".to_owned(), Value::from(name.as_str()));
    let mut subnets: Vec<Value> = Vec::new();
    if let Some(mac) = mac_addrs.get(&name) {
        iface.insert("mac_address".to_owned(), Value::from(mac.as_str()));
    }

    for pre in ["IPV4", "IPV6"] {
        if get(&format!("{pre}ADDR")).is_none() {
            continue;
        }
        let cur_proto = get(&format!("{pre}PROTO")).unwrap_or(proto);
        // ipconfig's 'none' is called 'static' in a v1 subnet.
        let cur_proto = if cur_proto == "none" {
            "static"
        } else {
            cur_proto
        };

        let mut subnet = Object::new();
        subnet.insert("type".to_owned(), Value::from(cur_proto));
        subnet.insert("control".to_owned(), Value::from("manual"));
        if cur_proto == "static" {
            if let Some(addr) = get(&format!("{pre}ADDR")) {
                subnet.insert("address".to_owned(), Value::from(addr));
            }
        }
        for key in ["NETMASK", "BROADCAST", "GATEWAY"] {
            if let Some(value) = get(&format!("{pre}{key}")) {
                subnet.insert(key.to_lowercase(), Value::from(value));
            }
        }

        let dns: Vec<Value> = ["DNS0", "DNS1"]
            .into_iter()
            .filter_map(|nskey| get(&format!("{pre}{nskey}")))
            // An all-zero address is how ipconfig spells "unset".
            .filter(|ns| !ns.trim_matches([':', '.', '0']).is_empty())
            .map(Value::from)
            .collect();
        if !dns.is_empty() {
            subnet.insert("dns_nameservers".to_owned(), Value::Array(dns));
            // The search list has no v4/v6 namespace, so it goes on both.
            if let Some(search) = get("DOMAINSEARCH") {
                let parts: Vec<Value> = if search.contains(',') {
                    search.split(',').map(Value::from).collect()
                } else {
                    search.split_whitespace().map(Value::from).collect()
                };
                subnet.insert("dns_search".to_owned(), Value::Array(parts));
            }
        }
        subnets.push(Value::Object(subnet));
    }

    iface.insert("subnets".to_owned(), Value::Array(subnets));
    Ok((name, iface))
}

/// `config_from_klibc_net_cfg`: fold the files into one v1 config.
///
/// # Errors
///
/// Anything [`klibc_to_config_entry`] raises, plus the two files that name one
/// device with two different macs.
pub fn config_from_klibc_net_cfg(
    files: &[PathBuf],
    mac_addrs: &BTreeMap<String, String>,
) -> Result<Object, KlibcError> {
    let mut entries: Vec<Object> = Vec::new();
    // Where each device landed in `entries`, and which files described it.
    let mut names: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();

    for path in files {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let (name, entry) = klibc_to_config_entry(&content, mac_addrs)?;
        let shown = path.display().to_string();
        if let Some((index, seen)) = names.get_mut(&name) {
            let Some(prev) = entries.get_mut(*index) else {
                continue;
            };
            let old = prev.get("mac_address").and_then(Value::as_str);
            let new = entry.get("mac_address").and_then(Value::as_str);
            if old != new {
                return Err(KlibcError::MultipleMacs {
                    name,
                    files: seen.join(" "),
                    old: py_str(old),
                    new: py_str(new),
                });
            }
            let extra = entry.get("subnets").and_then(Value::as_array).cloned();
            if let (Some(Value::Array(target)), Some(extra)) =
                (prev.get_mut("subnets"), extra)
            {
                target.extend(extra);
            }
            seen.push(shown);
        } else {
            names.insert(name, (entries.len(), vec![shown]));
            entries.push(entry);
        }
    }

    let mut out = Object::new();
    out.insert(
        "config".to_owned(),
        Value::Array(entries.into_iter().map(Value::Object).collect()),
    );
    out.insert("version".to_owned(), Value::from(1));
    Ok(out)
}

/// `KlibcNetworkConfigSource`, the only `_INITRAMFS_CONFIG_SOURCES` entry.
///
/// The three underscore-prefixed constructor arguments upstream carries "to
/// make testing simpler" become the fields, so a fixture can stand in for
/// `/run` without a global.
#[derive(Debug)]
pub struct Klibc {
    run_dir: PathBuf,
    files: Vec<PathBuf>,
    mac_addrs: BTreeMap<String, String>,
    cmdline: String,
}

impl Klibc {
    /// `_get_klibc_net_cfg_files`, sorted: upstream leaves it on `glob`, whose
    /// order is the filesystem's, and the order decides which file's subnets
    /// come first when two describe one device.
    #[must_use]
    pub fn net_cfg_files(run_dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(run_dir) else {
            return Vec::new();
        };
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".conf") else {
                continue;
            };
            if stem.starts_with("net-") {
                v4.push(entry.path());
            } else if stem.starts_with("net6-") {
                v6.push(entry.path());
            }
        }
        v4.sort();
        v6.sort();
        v4.extend(v6);
        v4
    }

    #[must_use]
    pub fn new(
        run_dir: impl Into<PathBuf>,
        cmdline: &str,
        sys: &crate::sysfs::Sys,
    ) -> Self {
        let run_dir = run_dir.into();
        let mac_addrs = sys
            .devicelist()
            .into_iter()
            .filter_map(|name| sys.read(&name, "address").map(|mac| (name, mac)))
            .collect();
        Self {
            files: Self::net_cfg_files(&run_dir),
            run_dir,
            mac_addrs,
            cmdline: cmdline.to_owned(),
        }
    }

    /// `is_applicable`: klibc files exist, and either the kernel was told to
    /// configure the network or iBFT did it without being asked.
    #[must_use]
    pub fn is_applicable(&self) -> bool {
        if self.files.is_empty() {
            return false;
        }
        // An unbalanced quote on the command line is a `ValueError` upstream
        // rather than a "no": there is nothing sensible to fall back to.
        let tokens = ci_core::shlex::split(&self.cmdline, false).unwrap_or_default();
        tokens
            .iter()
            .any(|item| item.starts_with("ip=") || item.starts_with("ip6="))
            || self.run_dir.join(OPEN_ISCSI_INTERFACE).exists()
    }

    /// `render_config`.
    ///
    /// # Errors
    ///
    /// Anything [`config_from_klibc_net_cfg`] raises.
    pub fn render_config(&self) -> Result<Object, KlibcError> {
        config_from_klibc_net_cfg(&self.files, &self.mac_addrs)
    }
}

/// `read_initramfs_config`.
///
/// # Errors
///
/// Anything the klibc reader raises. Upstream lets these escape into
/// `Init._find_networking_config` and take the stage down; the port's caller
/// logs and moves on — see docs/COMPAT.md.
pub fn read_initramfs_config(
    run_dir: &Path,
    cmdline: &str,
    sys: &crate::sysfs::Sys,
) -> Result<Option<Object>, KlibcError> {
    let source = Klibc::new(run_dir, cmdline, sys);
    if !source.is_applicable() {
        return Ok(None);
    }
    source.render_config().map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn read(cmdline: &str) -> Option<Object> {
        read_kernel_cmdline_config(cmdline, &mut ci_log::Logger::silent())
    }

    #[test]
    fn a_line_without_the_key_yields_nothing() {
        assert!(read("root=/dev/sda1 ro quiet").is_none());
    }

    #[test]
    fn the_disabled_sentinel_is_not_base64() {
        let cfg = read("ro network-config=disabled quiet").unwrap();
        assert!(is_disabled_cfg(Some(&cfg)));
    }

    #[test]
    fn a_plain_base64_yaml_payload_is_decoded() {
        let payload =
            ci_core::b64::encode(b"version: 2\nethernets: {eth0: {dhcp4: true}}\n");
        let cfg = read(&format!("ro network-config={payload}")).unwrap();
        assert_eq!(cfg.get("version").and_then(Value::as_i64), Some(2));
    }

    /// Upstream takes the last occurrence, not the first.
    #[test]
    fn the_last_occurrence_wins() {
        let first = ci_core::b64::encode(b"version: 1\n");
        let last = ci_core::b64::encode(b"version: 2\n");
        let cfg =
            read(&format!("network-config={first} network-config={last}")).unwrap();
        assert_eq!(cfg.get("version").and_then(Value::as_i64), Some(2));
    }

    /// And it takes it before testing it, so an empty one at the end wins too.
    #[test]
    fn a_trailing_empty_value_cancels_an_earlier_one() {
        let first = ci_core::b64::encode(b"version: 1\n");
        assert!(read(&format!("network-config={first} network-config=")).is_none());
    }

    #[test]
    fn an_undecodable_payload_is_dropped_rather_than_raised() {
        assert!(read("network-config=not!valid!base64").is_none());
    }

    fn entry(content: &str) -> (String, Object) {
        klibc_to_config_entry(content, &BTreeMap::new()).unwrap()
    }

    fn subnets(content: &str) -> Vec<Value> {
        entry(content)
            .1
            .get("subnets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap()
    }

    #[test]
    fn a_dhcp_file_becomes_a_physical_with_one_dhcp_subnet() {
        let (name, iface) = entry("DEVICE=eth0\nPROTO=dhcp\nIPV4ADDR=10.0.0.5\n");
        assert_eq!(name, "eth0");
        assert_eq!(iface.get("type").and_then(Value::as_str), Some("physical"));
        // dhcp subnets carry no address even though the file has one.
        assert_eq!(
            subnets("DEVICE=eth0\nPROTO=dhcp\nIPV4ADDR=10.0.0.5\n")
                .remove(0)
                .get("address"),
            None
        );
    }

    /// The v6 half names its device in `DEVICE6`, not `DEVICE`.
    #[test]
    fn the_device_name_may_come_from_either_key() {
        assert_eq!(entry("DEVICE6=eth0\nIPV6PROTO=dhcp6\n").0, "eth0");
        assert_eq!(
            klibc_to_config_entry("PROTO=dhcp\n", &BTreeMap::new()),
            Err(KlibcError::NoDevice)
        );
    }

    /// ipconfig calls a static config `none`; a v1 subnet calls it `static`.
    #[test]
    fn the_static_spellings_all_fold_together() {
        for proto in ["none", "static", "off"] {
            let content = format!("DEVICE=eth0\nPROTO={proto}\nIPV4ADDR=10.0.0.5\n");
            let subnet = subnets(&content).remove(0);
            assert_eq!(subnet.get("type").and_then(Value::as_str), Some("static"));
            assert_eq!(
                subnet.get("address").and_then(Value::as_str),
                Some("10.0.0.5")
            );
        }
    }

    /// Precise's ipconfig wrote no PROTO, so a boot filename is the only hint
    /// that a lease was involved.
    #[test]
    fn a_missing_proto_is_read_off_the_boot_filename() {
        assert_eq!(
            subnets("DEVICE=eth0\nIPV4ADDR=10.0.0.5\nfilename=pxelinux.0\n")
                .remove(0)
                .get("type")
                .and_then(Value::as_str),
            Some("dhcp")
        );
        assert_eq!(
            subnets("DEVICE=eth0\nIPV4ADDR=10.0.0.5\n")
                .remove(0)
                .get("type")
                .and_then(Value::as_str),
            Some("static")
        );
    }

    /// `bootp` is what ipconfig writes after a BOOTP configuration that
    /// succeeded; upstream rejects it and takes the boot stage with it.
    /// See COMPAT.md B69.
    #[test]
    fn a_proto_outside_the_allow_list_is_an_error() {
        assert_eq!(
            klibc_to_config_entry("DEVICE=eth0\nPROTO=bootp\n", &BTreeMap::new()),
            Err(KlibcError::UnexpectedProto("bootp".to_owned()))
        );
    }

    /// An all-zero nameserver is how ipconfig spells "unset", in either family.
    #[test]
    fn zero_nameservers_are_dropped_and_take_the_search_list_with_them() {
        let subnet = subnets(
            "DEVICE=eth0\nPROTO=dhcp\nIPV4ADDR=10.0.0.5\n\
             IPV4DNS0=0.0.0.0\nIPV4DNS1=0.0.0.0\nDOMAINSEARCH=a.com\n",
        )
        .remove(0);
        assert_eq!(subnet.get("dns_nameservers"), None);
        assert_eq!(subnet.get("dns_search"), None);

        let subnet = subnets(
            "DEVICE6=eth0\nIPV6PROTO=dhcp6\nIPV6ADDR=2001:db8::5\nIPV6DNS0=::\n",
        )
        .remove(0);
        assert_eq!(subnet.get("dns_nameservers"), None);
    }

    /// A comma splits, and without one the split is on whitespace.
    #[test]
    fn the_search_list_splits_on_commas_or_spaces() {
        for (raw, want) in [
            ("a.com,b.com", ["a.com", "b.com"]),
            ("'a.com b.com'", ["a.com", "b.com"]),
        ] {
            let subnet = subnets(&format!(
                "DEVICE=eth0\nPROTO=dhcp\nIPV4ADDR=10.0.0.5\n\
                 IPV4DNS0=10.0.0.1\nDOMAINSEARCH={raw}\n"
            ))
            .remove(0);
            assert_eq!(
                subnet.get("dns_search").and_then(Value::as_array).unwrap(),
                &want.map(Value::from).to_vec()
            );
        }
    }

    #[test]
    fn a_known_device_gets_its_mac_annotated() {
        let macs =
            BTreeMap::from([("eth0".to_owned(), "aa:bb:cc:dd:ee:ff".to_owned())]);
        let (_, iface) =
            klibc_to_config_entry("DEVICE=eth0\nPROTO=dhcp\n", &macs).unwrap();
        assert_eq!(
            iface.get("mac_address").and_then(Value::as_str),
            Some("aa:bb:cc:dd:ee:ff")
        );
    }

    fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    /// Two files for one device merge into one entry, v4 subnets first,
    /// because `net-*` sorts ahead of `net6-*`.
    #[test]
    fn a_device_described_twice_collects_both_subnets() {
        let dir = fixture(&[
            (
                "net-eth0.conf",
                "DEVICE=eth0\nPROTO=dhcp\nIPV4ADDR=10.0.0.5\n",
            ),
            (
                "net6-eth0.conf",
                "DEVICE6=eth0\nIPV6PROTO=dhcp6\nIPV6ADDR=2001:db8::5\n",
            ),
        ]);
        let cfg = config_from_klibc_net_cfg(
            &Klibc::net_cfg_files(dir.path()),
            &BTreeMap::new(),
        )
        .unwrap();

        let entries = cfg.get("config").and_then(Value::as_array).unwrap();
        assert_eq!(entries.len(), 1);
        let mut subnets = entries
            .first()
            .and_then(|entry| entry.get("subnets"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap();
        assert_eq!(subnets.len(), 2);
        let v6 = subnets.remove(1);
        let v4 = subnets.remove(0);
        assert_eq!(v4.get("type").and_then(Value::as_str), Some("dhcp"));
        assert_eq!(v6.get("type").and_then(Value::as_str), Some("dhcp6"));
        assert_eq!(cfg.get("version").and_then(Value::as_i64), Some(1));
    }

    /// Files that are not klibc's are not read, and a directory with none of
    /// them makes the source inapplicable however the kernel was called.
    #[test]
    fn only_the_two_klibc_globs_are_picked_up() {
        let dir = fixture(&[
            ("net-eth0.conf", "DEVICE=eth0\nPROTO=dhcp\n"),
            ("net6-eth0.conf", "DEVICE6=eth0\nIPV6PROTO=dhcp6\n"),
            ("net-eth0.conf.bak", "DEVICE=nope\n"),
            ("network-eth0.conf", "DEVICE=nope\n"),
            ("net-eth0.cfg", "DEVICE=nope\n"),
        ]);
        let names: Vec<String> = Klibc::net_cfg_files(dir.path())
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["net-eth0.conf", "net6-eth0.conf"]);
    }

    fn applicable(dir: &Path, cmdline: &str) -> bool {
        Klibc::new(dir, cmdline, &crate::sysfs::Sys::at(dir.join("nosuch")))
            .is_applicable()
    }

    #[test]
    fn the_source_needs_both_files_and_a_reason_to_have_run() {
        let empty = fixture(&[]);
        assert!(!applicable(empty.path(), "ip=dhcp"));

        let dir = fixture(&[("net-eth0.conf", "DEVICE=eth0\nPROTO=dhcp\n")]);
        assert!(applicable(dir.path(), "root=/dev/sda1 ip=dhcp ro"));
        assert!(applicable(dir.path(), "ip6=auto"));
        assert!(!applicable(dir.path(), "root=/dev/sda1 ro"));
        // A substring is not a token: `nfsroot=` must not look like `ip=`.
        assert!(!applicable(dir.path(), "root=/dev/sda1 zip=dhcp"));
    }

    /// iBFT configures the network without anything on the command line, and
    /// leaves this file to say so.
    #[test]
    fn an_open_iscsi_interface_file_stands_in_for_the_cmdline() {
        let dir = fixture(&[("net-eth0.conf", "DEVICE=eth0\nPROTO=dhcp\n")]);
        assert!(!applicable(dir.path(), "quiet"));
        std::fs::create_dir_all(dir.path().join("initramfs")).unwrap();
        std::fs::write(dir.path().join(OPEN_ISCSI_INTERFACE), "eth0\n").unwrap();
        assert!(applicable(dir.path(), "quiet"));
    }

    #[test]
    fn an_inapplicable_source_reads_nothing() {
        let dir = fixture(&[("net-eth0.conf", "DEVICE=eth0\nPROTO=bootp\n")]);
        let sys = crate::sysfs::Sys::at(dir.path().join("nosuch"));
        assert_eq!(read_initramfs_config(dir.path(), "quiet", &sys), Ok(None));
        // ...but an applicable one surfaces the file's own error.
        assert_eq!(
            read_initramfs_config(dir.path(), "ip=dhcp", &sys),
            Err(KlibcError::UnexpectedProto("bootp".to_owned()))
        );
    }
}
