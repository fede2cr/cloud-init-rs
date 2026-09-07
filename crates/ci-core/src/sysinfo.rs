//! `util.system_info` and the distro detection under it.
//!
//! Upstream reads this out of Python's `platform` module. There is no
//! interpreter here, so the same facts come from `/proc/sys/kernel` and
//! `/etc/os-release`; the two fields that describe the interpreter itself have
//! no answer at all (COMPAT.md deviation 66).

use std::collections::BTreeMap;
use std::path::Path;

use ci_config::{Object, Value};

/// Distributions `_get_variant` passes through unchanged.
const VARIANT_IS_DISTRO: [&str; 22] = [
    "almalinux",
    "alpine",
    "aosc",
    "arch",
    "azurelinux",
    "centos",
    "cloudlinux",
    "debian",
    "eurolinux",
    "fedora",
    "mariner",
    "miraclelinux",
    "openeuler",
    "opencloudos",
    "openmandriva",
    "photon",
    "raspberry-pi-os",
    "rhel",
    "rocky",
    "suse",
    "tencentos",
    "virtuozzo",
];

/// Distributions `_get_variant` folds into `suse`.
const VARIANT_IS_SUSE: [&str; 7] = [
    "opensuse",
    "opensuse-leap",
    "opensuse-microos",
    "opensuse-tumbleweed",
    "sle_hpc",
    "sle-micro",
    "sles",
];

/// `util.system_info()`.
#[must_use]
pub fn system_info() -> Object {
    system_info_from(Path::new("/proc/sys/kernel"), Path::new("/etc/os-release"))
}

fn system_info_from(proc_kernel: &Path, os_release: &Path) -> Object {
    let uname = uname(proc_kernel);
    let dist = linux_distro_from(os_release);
    let system = uname.first().cloned().unwrap_or_default();
    let release = uname.get(2).cloned().unwrap_or_default();
    let machine = uname.get(4).cloned().unwrap_or_default();

    let mut info = Object::new();
    info.insert(
        "platform".to_owned(),
        Value::String(format!("{system}-{release}-{machine}")),
    );
    info.insert("system".to_owned(), Value::String(system.clone()));
    info.insert("release".to_owned(), Value::String(release));
    info.insert("python".to_owned(), Value::String(String::new()));
    info.insert(
        "uname".to_owned(),
        Value::Array(uname.into_iter().map(Value::String).collect()),
    );
    info.insert(
        "dist".to_owned(),
        Value::Array(vec![
            Value::String(dist.0.clone()),
            Value::String(dist.1),
            Value::String(dist.2),
        ]),
    );
    info.insert(
        "variant".to_owned(),
        Value::String(variant(&system, &dist.0)),
    );
    info
}

/// `list(platform.uname())`: sysname, nodename, release, version, machine and
/// the processor, which is always empty on Linux.
fn uname(proc_kernel: &Path) -> Vec<String> {
    let read = |name: &str| {
        std::fs::read_to_string(proc_kernel.join(name))
            .map(|text| text.trim_end_matches('\n').to_owned())
            .unwrap_or_default()
    };
    vec![
        read("ostype"),
        read("hostname"),
        read("osrelease"),
        read("version"),
        std::env::consts::ARCH.to_owned(),
        String::new(),
    ]
}

/// `util.is_x86`.
///
/// Upstream reads `os.uname()[4]` and matches `i?86` as well as `x86_64`;
/// Rust spells the whole 32-bit family `x86`, so the two forms collapse.
#[must_use]
pub fn is_x86() -> bool {
    matches!(std::env::consts::ARCH, "x86_64" | "x86")
}

/// `util.get_linux_distro()`, as the `(name, version, flavor)` triple.
#[must_use]
pub fn linux_distro() -> (String, String, String) {
    linux_distro_from(Path::new("/etc/os-release"))
}

fn linux_distro_from(os_release: &Path) -> (String, String, String) {
    let Ok(text) = std::fs::read_to_string(os_release) else {
        // Upstream falls back to `/etc/redhat-release`, then to the BSD
        // branch; neither applies to a Linux-only port.
        return (String::new(), String::new(), String::new());
    };
    let fields = load_shell_content(&text);
    let get = |key: &str| fields.get(key).cloned().unwrap_or_default();

    let mut name = get("ID");
    if Path::new("/etc/rpi-issue").exists() {
        "raspberry-pi-os".clone_into(&mut name);
    }
    let version = get("VERSION_ID");
    let flavor = if name.contains("sles") || name.contains("suse") {
        std::env::consts::ARCH.to_owned()
    } else if name == "alpine" || name == "photon" || name == "virtuozzo" {
        get("PRETTY_NAME")
    } else {
        let codename = get("VERSION_CODENAME");
        if codename.is_empty() {
            codename_from_version(&get("VERSION"))
        } else {
            codename
        }
    };
    if name == "rhel" {
        "redhat".clone_into(&mut name);
    }
    (name, version, flavor)
}

/// The `r"[^ ]+ \((?P<codename>[^)]+)\)"` fallback: a codename in parentheses
/// after a leading word.
fn codename_from_version(version: &str) -> String {
    let Some((head, rest)) = version.split_once(" (") else {
        return String::new();
    };
    if head.is_empty() || head.contains(' ') {
        return String::new();
    }
    rest.split_once(')')
        .map(|(codename, _)| codename.to_owned())
        .filter(|codename| !codename.is_empty())
        .unwrap_or_default()
}

/// `util._get_variant`.
#[must_use]
pub fn variant(system: &str, distro: &str) -> String {
    if !system.eq_ignore_ascii_case("linux") {
        // The BSD and Windows branches cannot be reached from `/proc`.
        return "unknown".to_owned();
    }
    let distro = distro.to_lowercase();
    if VARIANT_IS_DISTRO.contains(&distro.as_str()) {
        distro
    } else if matches!(distro.as_str(), "ubuntu" | "linuxmint" | "mint") {
        "ubuntu".to_owned()
    } else if distro == "redhat" {
        "rhel".to_owned()
    } else if VARIANT_IS_SUSE.contains(&distro.as_str()) {
        "suse".to_owned()
    } else {
        "linux".to_owned()
    }
}

/// `util.system_is_snappy`.
///
/// Four guesses in a row, any of which is enough. Upstream calls it "certainly
/// not a perfect test, but good enough for now" and it has stayed that way; it
/// decides whether accounts land in `/var/lib/extrausers` instead of `/etc`,
/// so a wrong answer here creates a user nobody can log in as.
#[must_use]
pub fn system_is_snappy(root: &Path) -> bool {
    let os_release =
        std::fs::read_to_string(root.join("etc/os-release")).unwrap_or_default();
    if load_shell_content(&os_release)
        .get("ID")
        .is_some_and(|id| id.to_lowercase() == "ubuntu-core")
    {
        return true;
    }
    if ci_config::cmdline::get_cmdline().contains("snap_core=") {
        return true;
    }
    let channel = std::fs::read_to_string(root.join("etc/system-image/channel.ini"))
        .unwrap_or_default();
    if channel.to_lowercase().contains("ubuntu-core") {
        return true;
    }
    root.join("etc/system-image/config.d").is_dir()
}

/// `util.kernel_version()`.
///
/// A `Vec` rather than a pair because upstream builds a Python tuple from
/// `release.split(".")[:2]`, and a release with no `.` in it yields a
/// ONE-element tuple. Rust compares `Vec` lexicographically exactly as Python
/// compares tuples, so `kernel_version()? < vec![4, 18]` is the same test.
///
/// # Errors
/// The `int()` upstream never guards, as its `ValueError` message.
pub fn kernel_version() -> Result<Vec<u32>, String> {
    kernel_version_of(
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim_end_matches('\n'),
    )
}

fn kernel_version_of(release: &str) -> Result<Vec<u32>, String> {
    release
        .split('.')
        .take(2)
        .map(|part| part.parse::<u32>().map_err(|_| int_error(part)))
        .collect()
}

/// The three keys `util.read_meminfo` reports. Each is `None` when
/// `/proc/meminfo` did not name it, which is upstream's missing dict key.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Meminfo {
    pub total: Option<u64>,
    pub free: Option<u64>,
    pub available: Option<u64>,
}

/// `util.read_meminfo`, without its `raw` mode (COMPAT.md deviation 147).
///
/// # Errors
/// The unpack, `int()` and multiplier-lookup failures upstream leaves
/// uncaught, each as the message Python would print.
pub fn read_meminfo(path: &Path) -> Result<Meminfo, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| crate::pyerr::oserror(&err, path))?;
    parse_meminfo(&text)
}

fn parse_meminfo(text: &str) -> Result<Meminfo, String> {
    let mut out = Meminfo::default();
    for line in crate::pystr::split_lines(text) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Upstream unpacks into three names and retries with two; whichever
        // way it fails, the message that escapes is the two-name one.
        let (key, value, unit) = match fields.as_slice() {
            [key, value, unit] => (*key, *value, *unit),
            [key, value] => (*key, *value, "B"),
            other => {
                let n = other.len();
                let what = if n < 2 { "not enough" } else { "too many" };
                return Err(format!("{what} values to unpack (expected 2, got {n})"));
            }
        };
        // With `raw` gone, a line outside the map is skipped before its unit
        // or its value is ever looked at.
        let slot = match key {
            "MemTotal:" => &mut out.total,
            "MemFree:" => &mut out.free,
            "MemAvailable:" => &mut out.available,
            _ => continue,
        };
        let mplier: u64 = match unit {
            "B" => 1,
            "kB" => 1 << 10,
            "mB" => 1 << 20,
            "gB" => 1 << 30,
            other => return Err(format!("'{other}'")),
        };
        let amount: u64 = value.parse().map_err(|_| int_error(value))?;
        // Python has bignums; /proc/meminfo counts kB, so the ceiling is out
        // of reach and saturating costs nothing.
        *slot = Some(amount.saturating_mul(mplier));
    }
    Ok(out)
}

fn int_error(text: &str) -> String {
    format!("invalid literal for int() with base 10: '{text}'")
}

/// `util.load_shell_content`, narrowed to what `/etc/os-release` uses: one
/// `KEY=value` per line, with the value optionally quoted.
fn load_shell_content(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        out.insert(key.trim().to_owned(), unquote(value.trim()));
    }
    out
}

fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner.to_owned();
        }
    }
    value.to_owned()
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

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_kernel_release_keeps_its_first_two_components() {
        assert_eq!(kernel_version_of("6.14.0-33-generic").unwrap(), vec![6, 14]);
        assert_eq!(kernel_version_of("6.14").unwrap(), vec![6, 14]);
    }

    #[test]
    fn a_release_with_no_dot_yields_a_one_element_tuple() {
        // Upstream's `(6,) < (4, 18)` is False; `Vec` compares the same way.
        let one = kernel_version_of("6").unwrap();
        assert_eq!(one, vec![6]);
        assert!(one >= vec![4, 18]);
        assert!(kernel_version_of("4").unwrap() < vec![4, 18]);
    }

    #[test]
    fn a_release_component_that_is_not_a_number_is_upstreams_value_error() {
        assert_eq!(
            kernel_version_of("6-generic").unwrap_err(),
            "invalid literal for int() with base 10: '6-generic'"
        );
        assert_eq!(
            kernel_version_of("6.x.1").unwrap_err(),
            "invalid literal for int() with base 10: 'x'"
        );
    }

    #[test]
    fn meminfo_scales_by_the_unit_column() {
        let info = parse_meminfo(
            "MemTotal:       32714380 kB\n\
             MemFree:            3832 mB\n\
             MemAvailable:          1 gB\n\
             HugePages_Total:       0\n",
        )
        .unwrap();
        assert_eq!(info.total, Some(32_714_380 * 1024));
        assert_eq!(info.free, Some(3832 * 1024 * 1024));
        assert_eq!(info.available, Some(1 << 30));
    }

    #[test]
    fn a_key_outside_the_map_is_skipped_before_its_unit_is_read() {
        // `Hugepagesize: 2048 zB` would be a KeyError under `raw=True`; with
        // `raw` gone the line never reaches the multiplier lookup.
        let info = parse_meminfo("Hugepagesize: 2048 zB\nMemTotal: 4 B\n").unwrap();
        assert_eq!(info.total, Some(4));
        assert_eq!(info.free, None);
    }

    #[test]
    fn an_unknown_unit_on_a_mapped_key_is_upstreams_key_error() {
        assert_eq!(parse_meminfo("MemTotal: 100 zB\n").unwrap_err(), "'zB'");
    }

    #[test]
    fn a_line_that_will_not_unpack_reports_the_two_name_attempt() {
        assert_eq!(
            parse_meminfo("MemTotal: 100 kB extra\n").unwrap_err(),
            "too many values to unpack (expected 2, got 4)"
        );
        assert_eq!(
            parse_meminfo("\n").unwrap_err(),
            "not enough values to unpack (expected 2, got 0)"
        );
        assert_eq!(
            parse_meminfo("MemTotal:\n").unwrap_err(),
            "not enough values to unpack (expected 2, got 1)"
        );
    }

    #[test]
    fn a_value_that_is_not_a_number_is_upstreams_value_error() {
        assert_eq!(
            parse_meminfo("MemTotal: abc kB\n").unwrap_err(),
            "invalid literal for int() with base 10: 'abc'"
        );
    }

    #[test]
    fn the_uname_tuple_comes_out_of_proc_in_platform_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ostype", "Linux\n");
        write(dir.path(), "hostname", "howlern\n");
        write(dir.path(), "osrelease", "6.18.0\n");
        write(dir.path(), "version", "#1 SMP Thu Jun 18\n");

        let uname = uname(dir.path());
        assert_eq!(uname[0], "Linux");
        assert_eq!(uname[1], "howlern");
        assert_eq!(uname[2], "6.18.0");
        assert_eq!(uname[3], "#1 SMP Thu Jun 18");
        assert_eq!(uname[4], std::env::consts::ARCH);
        assert_eq!(uname[5], "");
    }

    #[test]
    fn os_release_values_are_unquoted() {
        let fields =
            load_shell_content("ID=ubuntu\nVERSION_ID=\"26.04\"\n# a comment\n\n");
        assert_eq!(fields["ID"], "ubuntu");
        assert_eq!(fields["VERSION_ID"], "26.04");
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn the_codename_falls_back_to_the_one_in_parentheses() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("os-release");
        std::fs::write(
            &file,
            "ID=debian\nVERSION_ID=\"12\"\nVERSION=\"12 (bookworm)\"\n",
        )
        .unwrap();
        assert_eq!(
            linux_distro_from(&file),
            ("debian".to_owned(), "12".to_owned(), "bookworm".to_owned())
        );
    }

    #[test]
    fn rhel_is_reported_as_redhat_but_its_variant_is_rhel_again() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("os-release");
        std::fs::write(&file, "ID=rhel\nVERSION_ID=9.4\n").unwrap();
        let dist = linux_distro_from(&file);
        assert_eq!(dist.0, "redhat");
        assert_eq!(variant("Linux", &dist.0), "rhel");
    }

    #[test]
    fn an_unknown_distribution_is_still_a_linux() {
        assert_eq!(variant("Linux", "nixos"), "linux");
        assert_eq!(variant("Linux", "sles"), "suse");
        assert_eq!(variant("Linux", "linuxmint"), "ubuntu");
        assert_eq!(variant("Darwin", "whatever"), "unknown");
    }

    #[test]
    fn a_missing_os_release_leaves_the_distro_empty_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            linux_distro_from(&dir.path().join("absent")),
            (String::new(), String::new(), String::new())
        );
    }

    #[test]
    fn the_platform_string_is_the_one_field_the_port_cannot_reproduce() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ostype", "Linux\n");
        write(dir.path(), "osrelease", "6.18.0\n");
        let info = system_info_from(dir.path(), &dir.path().join("absent"));
        // Upstream appends `-with-glibc<x>.<y>`, which it reads out of the
        // interpreter binary.
        assert_eq!(
            info["platform"],
            Value::String(format!("Linux-6.18.0-{}", std::env::consts::ARCH))
        );
        assert_eq!(info["python"], Value::String(String::new()));
    }
}
