//! Port of `cc_scripts_user`, `cc_scripts_vendor`, `cc_scripts_per_once`,
//! `cc_scripts_per_boot` and `cc_scripts_per_instance`.
//!
//! Five upstream files that differ only in which directory they hand to
//! `subp.runparts`, so they are one file here. This is the execution end of
//! the user-data pipeline: whatever the `#!`-script handler, `cc_runcmd` and
//! the vendor-data handler dropped into those directories runs now, as root,
//! with no further inspection. The module therefore has no config of its own —
//! the only thing that decides what runs is the contents of a directory.

use std::path::{Path, PathBuf};

use ci_config::Value;
use ci_sys::subp::Subp;

use super::Args;

/// `cc_scripts_user`: `<instance>/scripts`, where the script part-handler and
/// `cc_runcmd` write.
pub fn user(args: &mut Args<'_>) -> Result<(), String> {
    let dir = args.paths.instance_link().join("scripts");
    run_dir(args, "cc_scripts_user.py", "scripts", &dir, &[])
}

/// `cc_scripts_vendor`: `<instance>/scripts/vendor`, optionally under a
/// wrapper command the *config* — not the vendor data — names.
pub fn vendor(args: &mut Args<'_>) -> Result<(), String> {
    let dir = args.paths.instance_link().join("scripts").join("vendor");
    let prefix = exe_prefix(args.cfg.get("vendor_data"))?;
    run_dir(args, "cc_scripts_vendor.py", "vendor", &dir, &prefix)
}

/// `cc_scripts_per_once`: `<cloud_dir>/scripts/per-once`.
pub fn per_once(args: &mut Args<'_>) -> Result<(), String> {
    per_dir(args, "cc_scripts_per_once.py", "per-once")
}

/// `cc_scripts_per_boot`: `<cloud_dir>/scripts/per-boot`.
pub fn per_boot(args: &mut Args<'_>) -> Result<(), String> {
    per_dir(args, "cc_scripts_per_boot.py", "per-boot")
}

/// `cc_scripts_per_instance`: `<cloud_dir>/scripts/per-instance`.
pub fn per_instance(args: &mut Args<'_>) -> Result<(), String> {
    per_dir(args, "cc_scripts_per_instance.py", "per-instance")
}

/// The three `per-*` modules, which differ only in the last path component.
/// They read `get_cpath()`, not the instance directory, so their scripts
/// survive a re-provision — which is what makes `per-once` mean anything.
fn per_dir(args: &mut Args<'_>, source: &str, subdir: &str) -> Result<(), String> {
    let dir = args.paths.cloud_dir.join("scripts").join(subdir);
    run_dir(args, source, subdir, &dir, &[])
}

/// `util.get_cfg_by_path(cfg, ("vendor_data", "prefix"), [])` as `runparts`
/// then interprets it: a missing key means no prefix, a string is a
/// one-element prefix, and a list is used as-is.
///
/// The lookup itself is Python's `in`, so a `vendor_data` that is a string or
/// a list quietly yields the default, and one that is a scalar raises. Both
/// are reproduced — the second as an error, since silently running the vendor
/// scripts unwrapped is exactly the case a prefix exists to prevent.
fn exe_prefix(vendor_data: Option<&Value>) -> Result<Vec<String>, String> {
    match vendor_data {
        None | Some(Value::String(_) | Value::Array(_)) => return Ok(Vec::new()),
        Some(Value::Object(_)) => {}
        Some(other) => {
            return Err(format!(
                "argument of type '{}' is not a container or iterable",
                ci_config::type_name(other)
            ))
        }
    }
    let Some(prefix) = vendor_data.and_then(|v| v.get("prefix")) else {
        return Ok(Vec::new());
    };
    match prefix {
        Value::Null => Ok(Vec::new()),
        Value::String(one) => Ok(vec![one.clone()]),
        Value::Array(items) => Ok(items.iter().map(super::py_str).collect::<Vec<_>>()),
        other => Err(format!(
            "exe_prefix must be None, str, or list, not {}",
            ci_config::type_name(other)
        )),
    }
}

/// `subp.runparts(dirp, exe_prefix=...)`.
///
/// A missing directory is the normal case and is silent. Every executable file
/// directly inside it runs, in sorted order, with output going to this
/// process' own stdout and stderr — `capture=False` upstream. A failure does
/// not stop the run: the failures are collected and reported once at the end,
/// so a broken script cannot hide the ones after it.
fn run_dir(
    args: &mut Args<'_>,
    source: &str,
    label: &str,
    dir: &Path,
    prefix: &[String],
) -> Result<(), String> {
    match runparts(args, source, dir, prefix) {
        Ok(()) => Ok(()),
        Err(err) => {
            let name = args.name.to_owned();
            args.warning(
                source,
                &format!("Failed to run module {name} ({label} in {})", dir.display()),
            );
            Err(err)
        }
    }
}

fn runparts(
    args: &mut Args<'_>,
    source: &str,
    dir: &Path,
    prefix: &[String],
) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = sorted_entries(dir)
        .map_err(|err| format!("Failed to list {}: {err}", dir.display()))?;

    let mut attempted = 0usize;
    let mut failed = Vec::new();
    for path in entries {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if ci_sys::subp::is_exe(&path) {
            attempted += 1;
            let mut cmd: Vec<&std::ffi::OsStr> =
                prefix.iter().map(std::ffi::OsStr::new).collect();
            cmd.push(path.as_os_str());
            let status = Subp::new(cmd).inherit_env().passthrough();
            match status {
                Ok(status) if status.success() => {}
                _ => failed.push(name),
            }
        } else if path.is_file() {
            args.warning(
                source,
                &format!(
                    "skipping {} as its not executable or the underlying file \
                     system is mounted without executable permissions.",
                    path.display()
                ),
            );
        } else {
            args.debug(
                source,
                &format!("Not executing special file [{}]", path.display()),
            );
        }
    }

    if failed.is_empty() || attempted == 0 {
        return Ok(());
    }
    Err(format!(
        "Runparts: {} failures ({}) in {attempted} attempted commands",
        failed.len(),
        failed.join(",")
    ))
}

/// `sorted(os.listdir(dirp))`: every entry, not just the regular files, so
/// that a directory in there produces upstream's "special file" line.
fn sorted_entries(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    out.sort();
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use ci_log::Logger;
    use serde_json::json;

    use super::*;

    /// These tests write a script and then exec it. A sibling test that forks in
    /// between inherits the still-open write fd, so the exec fails with
    /// `ETXTBSY` and `runparts` reports the script as a failure — the same race
    /// `cloud-init-generator`'s tests hit. Every test here that writes a script
    /// or runs one takes this lock.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes `body` into `dir/name` and makes it executable unless `mode` says
    /// otherwise.
    fn script(dir: &Path, name: &str, body: &str, mode: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn run(
        root: &Path,
        cfg: &serde_json::Value,
        handler: super::super::Handler,
    ) -> Result<(), String> {
        let cfg = cfg.as_object().unwrap();
        let paths = ci_core::Paths {
            cloud_dir: root.join("cloud"),
            ..Default::default()
        };
        let mut logger = Logger::silent();
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "scripts",
            cfg,
            args: &empty,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(crate::cc::tests::fixture_datasource()),
            logger: &mut logger,
        };
        handler(&mut args)
    }

    #[test]
    fn a_directory_that_does_not_exist_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), &json!({}), per_boot), Ok(()));
    }

    #[test]
    fn executables_run_in_sorted_order() {
        let _guard = serialized();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let scripts = dir.path().join("cloud/scripts/per-boot");
        // Named so that lexical order and creation order disagree.
        script(
            &scripts,
            "20-second",
            &format!("#!/bin/sh\necho second >> {}\n", out.display()),
            0o755,
        );
        script(
            &scripts,
            "10-first",
            &format!("#!/bin/sh\necho first >> {}\n", out.display()),
            0o755,
        );
        assert_eq!(run(dir.path(), &json!({}), per_boot), Ok(()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn a_file_without_the_execute_bit_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let scripts = dir.path().join("cloud/scripts/per-once");
        script(
            &scripts,
            "inert",
            &format!("#!/bin/sh\necho ran >> {}\n", out.display()),
            0o644,
        );
        assert_eq!(run(dir.path(), &json!({}), per_once), Ok(()));
        assert!(!out.exists());
    }

    #[test]
    fn every_script_runs_even_after_one_fails() {
        let _guard = serialized();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let scripts = dir.path().join("cloud/scripts/per-instance");
        script(&scripts, "1-bad", "#!/bin/sh\nexit 3\n", 0o755);
        script(
            &scripts,
            "2-good",
            &format!("#!/bin/sh\necho ran >> {}\n", out.display()),
            0o755,
        );
        script(&scripts, "3-bad", "#!/bin/sh\nexit 4\n", 0o755);
        assert_eq!(
            run(dir.path(), &json!({}), per_instance),
            Err("Runparts: 2 failures (1-bad,3-bad) in 3 attempted commands".to_owned())
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "ran\n");
    }

    #[test]
    fn the_user_scripts_come_from_the_instance_link() {
        let _guard = serialized();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let real = dir.path().join("cloud/instances/i-test/scripts");
        script(
            &real,
            "go",
            &format!("#!/bin/sh\necho ran >> {}\n", out.display()),
            0o755,
        );
        std::os::unix::fs::symlink(
            "instances/i-test",
            dir.path().join("cloud/instance"),
        )
        .unwrap();
        assert_eq!(run(dir.path(), &json!({}), user), Ok(()));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "ran\n");
    }

    #[test]
    fn the_vendor_prefix_wraps_each_script() {
        let _guard = serialized();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let real = dir.path().join("cloud/instances/i-test/scripts/vendor");
        // Not executable: it only ever runs as an argument to the prefix.
        script(
            &real,
            "go",
            &format!("echo ran >> {}\n", out.display()),
            0o755,
        );
        std::os::unix::fs::symlink(
            "instances/i-test",
            dir.path().join("cloud/instance"),
        )
        .unwrap();
        assert_eq!(
            run(
                dir.path(),
                &json!({"vendor_data": {"prefix": ["/bin/sh"]}}),
                vendor
            ),
            Ok(())
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "ran\n");
    }

    #[test]
    fn a_prefix_that_is_not_none_a_string_or_a_list_is_a_type_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run(dir.path(), &json!({"vendor_data": {"prefix": 5}}), vendor),
            Err("exe_prefix must be None, str, or list, not int".to_owned())
        );
    }
}
