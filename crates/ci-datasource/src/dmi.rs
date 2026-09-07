//! Port of `cloudinit/dmi.py`.
//!
//! Only the Linux readers exist: `/sys/class/dmi/id` first, then `dmidecode`.
//! The FreeBSD `kenv` and OpenBSD `sysctl` columns of upstream's translation
//! table have no reader here, so they are not carried.

use std::path::Path;

use ci_log::Logger;

/// `DMI_SYS_PATH`.
const DMI_SYS_PATH: &str = "/sys/class/dmi/id";

/// `DMIDECODE_TO_KERNEL`, in upstream's declaration order, which is the order
/// the invalid-key warning prints them in.
const DMIDECODE_TO_KERNEL: [(&str, &str); 17] = [
    ("baseboard-asset-tag", "board_asset_tag"),
    ("baseboard-manufacturer", "board_vendor"),
    ("baseboard-product-name", "board_name"),
    ("baseboard-serial-number", "board_serial"),
    ("baseboard-version", "board_version"),
    ("bios-release-date", "bios_date"),
    ("bios-vendor", "bios_vendor"),
    ("bios-version", "bios_version"),
    ("chassis-asset-tag", "chassis_asset_tag"),
    ("chassis-manufacturer", "chassis_vendor"),
    ("chassis-serial-number", "chassis_serial"),
    ("chassis-version", "chassis_version"),
    ("system-manufacturer", "sys_vendor"),
    ("system-product-name", "product_name"),
    ("system-serial-number", "product_serial"),
    ("system-uuid", "product_uuid"),
    ("system-version", "product_version"),
];

fn kernel_name(key: &str) -> Option<&'static str> {
    DMIDECODE_TO_KERNEL
        .iter()
        .find(|(dmidecode, _)| *dmidecode == key)
        .map(|(_, kernel)| *kernel)
}

/// `read_dmi_data`.
///
/// Inside a container DMI describes the host, so upstream refuses to read it
/// at all rather than hand back somebody else's identity.
pub fn read_dmi_data(key: &str, logger: &mut Logger) -> Option<String> {
    if ci_core::container::is_container() {
        return None;
    }
    if let Some(value) = read_syspath(Path::new(DMI_SYS_PATH), key, logger) {
        return Some(value);
    }
    // The docstring promises a second pass using `key` as a sysfs name
    // directly. The code has never done that: an unknown key stops at the
    // translation table above.
    let arch = std::env::consts::ARCH;
    if !is_dmidecode_arch(arch) {
        logger.debug("dmi.py", &format!("dmidata is not supported on {arch}"));
        return None;
    }
    if let Some(path) = ci_sys::subp::which("dmidecode") {
        return call_dmidecode(key, &path, logger);
    }
    logger.debug(
        "dmi.py",
        &format!("did not find either path {DMI_SYS_PATH} or dmidecode command"),
    );
    None
}

/// `is_x86` widened by the two names upstream accepts alongside it. Rust spells
/// 32-bit x86 `x86` where `uname -m` says `i686`, so both forms are allowed.
fn is_dmidecode_arch(arch: &str) -> bool {
    matches!(arch, "x86_64" | "x86" | "aarch64" | "amd64")
        || (arch.starts_with('i') && arch.len() == 4 && arch.ends_with("86"))
}

/// `_read_dmi_syspath`.
fn read_syspath(root: &Path, key: &str, logger: &mut Logger) -> Option<String> {
    let name = kernel_name(key)?;
    let path = root.join(name);
    logger.debug("dmi.py", &format!("querying dmi data {}", path.display()));
    if !path.exists() {
        logger.debug("dmi.py", &format!("did not find {}", path.display()));
        return None;
    }
    let Ok(mut raw) = std::fs::read(&path) else {
        logger.debug("dmi.py", &format!("Could not read {}", path.display()));
        return None;
    };
    // An uninitialised DMI field reads as all 0xff, with the newline sysfs
    // appends; that is absence, not a value.
    if raw.len() > 1
        && raw.last() == Some(&b'\n')
        && raw
            .get(..raw.len() - 1)
            .is_some_and(|b| b.iter().all(|c| *c == 0xff))
    {
        raw.clear();
    }
    match String::from_utf8(raw) {
        Ok(text) => Some(text.trim().to_owned()),
        Err(e) => {
            logger.error(
                "dmi.py",
                &format!(
                    "utf-8 decode of content ({}) in {} failed: {e}",
                    path.display(),
                    path.display()
                ),
            );
            None
        }
    }
}

/// `_call_dmidecode`.
fn call_dmidecode(key: &str, program: &Path, logger: &mut Logger) -> Option<String> {
    let cmd = [
        program.as_os_str(),
        std::ffi::OsStr::new("--string"),
        key.as_ref(),
    ];
    match ci_sys::Subp::new(cmd).run() {
        Ok(out) if out.success() => {
            let result = out.stdout_trimmed();
            logger.debug(
                "dmi.py",
                &format!("dmidecode returned '{result}' for '{key}'"),
            );
            // A field of nothing but dots is how dmidecode spells "empty".
            if result.replace('.', "").is_empty() {
                return Some(String::new());
            }
            Some(result)
        }
        Ok(_) | Err(_) => {
            logger.debug(
                "dmi.py",
                &format!(
                    "failed dmidecode cmd: ['{}', '--string', '{key}']",
                    program.display()
                ),
            );
            None
        }
    }
}

/// `sub_dmi_vars`: replace `__dmi.KEY__` with what the machine reports.
pub fn sub_dmi_vars(src: &str, logger: &mut Logger) -> String {
    if !src.contains("__") {
        return src.to_owned();
    }
    let mut out = src.to_owned();
    // The key list is collected from the original string, as `re.findall`
    // does, so a substituted value cannot introduce another placeholder.
    for key in find_placeholders(src) {
        if kernel_name(&key).is_none() {
            logger.warning(
                "dmi.py",
                &format!(
                    "Ignoring invalid __dmi.{key}__ in {src}. Expected one of: \
                     {}.",
                    valid_keys_repr()
                ),
            );
            continue;
        }
        let value = read_dmi_data(&key, logger).unwrap_or_default();
        logger.debug(
            "dmi.py",
            &format!("Replacing __dmi.{key}__ in '{out}' with '{value}'."),
        );
        out = out.replace(&format!("__dmi.{key}__"), &value);
    }
    out
}

/// `re.findall(r"__dmi\.([^_]+)__", src)`: a run with no underscore in it,
/// closed by exactly two.
fn find_placeholders(src: &str) -> Vec<String> {
    const OPEN: &str = "__dmi.";
    let mut found = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find(OPEN) {
        let after = rest.get(at + OPEN.len()..).unwrap_or("");
        let run: String = after.chars().take_while(|c| *c != '_').collect();
        let tail = after.get(run.len()..).unwrap_or("");
        if !run.is_empty() && tail.starts_with("__") {
            found.push(run.clone());
            rest = tail.get(2..).unwrap_or("");
        } else {
            rest = after;
        }
    }
    found
}

/// `DMIDECODE_TO_KERNEL.keys()` as Python renders it inside a log message.
fn valid_keys_repr() -> String {
    let keys = DMIDECODE_TO_KERNEL
        .iter()
        .map(|(dmidecode, _)| format!("'{dmidecode}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("dict_keys([{keys}])")
}

/// The `dmidecode` names this build can translate, for callers that want to
/// list them.
#[must_use]
pub fn keys() -> Vec<&'static str> {
    DMIDECODE_TO_KERNEL
        .iter()
        .map(|(dmidecode, _)| *dmidecode)
        .collect()
}

/// `read_dmi_data` restricted to `/sys`, for a caller that supplies the root.
#[must_use]
pub fn read_syspath_at(root: &Path, key: &str, logger: &mut Logger) -> Option<String> {
    read_syspath(root, key, logger)
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
    fn a_placeholder_run_may_not_contain_an_underscore() {
        assert_eq!(
            find_placeholders("__dmi.system-serial-number__"),
            ["system-serial-number"]
        );
        assert_eq!(
            find_placeholders("__dmi.product_serial__"),
            Vec::<String>::new()
        );
        assert_eq!(find_placeholders("__dmi.x__ and __dmi.y__"), ["x", "y"]);
        assert!(find_placeholders("__dmi.__").is_empty());
    }

    #[test]
    fn an_unknown_key_is_left_alone() {
        let mut logger = Logger::silent();
        assert_eq!(
            sub_dmi_vars("http://h/__dmi.nope__/", &mut logger),
            "http://h/__dmi.nope__/"
        );
    }

    #[test]
    fn a_string_without_a_double_underscore_is_returned_untouched() {
        let mut logger = Logger::silent();
        assert_eq!(
            sub_dmi_vars("http://h/seed/", &mut logger),
            "http://h/seed/"
        );
    }

    #[test]
    fn an_uninitialised_sysfs_field_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("product_serial"), b"\xff\xff\xff\n").unwrap();
        let mut logger = Logger::silent();
        assert_eq!(
            read_syspath(dir.path(), "system-serial-number", &mut logger),
            Some(String::new())
        );
    }

    #[test]
    fn a_sysfs_field_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("product_serial"), b"ds=nocloud;s=/x\n")
            .unwrap();
        let mut logger = Logger::silent();
        assert_eq!(
            read_syspath(dir.path(), "system-serial-number", &mut logger),
            Some("ds=nocloud;s=/x".to_owned())
        );
    }

    #[test]
    fn an_untranslatable_key_never_touches_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let mut logger = Logger::silent();
        assert_eq!(
            read_syspath(dir.path(), "processor-family", &mut logger),
            None
        );
    }
}
