//! Port of `cloudinit/config/cc_runcmd.py`.
//!
//! The module's name is a trap: it does not run anything. It shellifies the
//! `runcmd` list into `<instance>/scripts/runcmd` and stops. What executes
//! that file is `cc_scripts_user`, later in the same boot, via `run-parts`.
//! Everything about this module's behaviour — including that a broken entry
//! fails here and not at execution time — follows from that split.

use ci_sys::atomic::{self, WriteOptions};

use super::{shellify, Args};

const SOURCE: &str = "cc_runcmd.py";

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let Some(commands) = args.cfg.get("runcmd").cloned() else {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!("Skipping module named {name}, no 'runcmd' key in configuration"),
        );
        return Ok(());
    };

    let Some(dir) = args.ipath(ci_core::paths::Lookup::Scripts) else {
        // Upstream joins `None` with "runcmd" and dies with a TypeError.
        return Err(
            "No instance directory is available to write runcmd into".to_owned()
        );
    };
    let out = dir.join("runcmd");

    let (content, made) = shellify(&commands).map_err(|err| {
        format!("Failed to shellify into file {}: {err}", out.display())
    })?;
    args.debug("util.py", &format!("Shellified {made} commands."));

    // `util.ensure_dir` with no mode: the umask decides, as for any mkdir.
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Failed to create {}: {err}", dir.display()))?;
    // 0700: the file is a root-run script that tenant data wrote.
    atomic::write_file(&out, content, WriteOptions::mode(0o700))
        .map_err(|err| format!("Failed to write file {}: {err}", out.display()))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::path::Path;

    use ci_config::Value;
    use ci_log::Logger;
    use serde_json::json;

    use super::*;

    fn run(root: &Path, cfg: &serde_json::Value) -> Result<(), String> {
        let cfg = cfg.as_object().unwrap();
        let paths = ci_core::Paths {
            cloud_dir: root.join("cloud"),
            ..Default::default()
        };
        let mut logger = Logger::silent();
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "runcmd",
            cfg,
            args: &empty,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(crate::cc::tests::fixture_datasource()),
            logger: &mut logger,
        };
        handle(&mut args)
    }

    fn script(root: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(root.join("cloud/instances/i-test/scripts/runcmd"))
    }

    #[test]
    fn a_config_without_the_key_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), &json!({})), Ok(()));
        assert!(script(dir.path()).is_err());
    }

    #[test]
    fn the_script_is_shellified_and_only_root_may_run_it() {
        let dir = tempfile::tempdir().unwrap();
        run(
            dir.path(),
            &json!({"runcmd": ["echo one", ["echo", "two"]]}),
        )
        .unwrap();
        let path = dir.path().join("cloud/instances/i-test/scripts/runcmd");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho one\n'echo' 'two'\n"
        );
        assert_eq!(ci_sys::ids::mode_of(&path).unwrap(), 0o700);
    }

    #[test]
    fn an_empty_list_still_writes_the_interpreter_line() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &json!({"runcmd": []})).unwrap();
        assert_eq!(script(dir.path()).unwrap(), "#!/bin/sh\n");
    }

    #[test]
    fn a_runcmd_that_is_not_a_list_fails_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), &json!({"runcmd": "echo hi"})).unwrap_err();
        assert!(err.contains("Expected list or tuple"), "{err}");
        assert!(script(dir.path()).is_err());
    }

    #[test]
    fn without_an_instance_there_is_nowhere_to_write() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ci_core::Paths::default();
        let mut logger = Logger::silent();
        let cfg = json!({"runcmd": ["echo hi"]});
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "runcmd",
            cfg: cfg.as_object().unwrap(),
            args: &empty,
            paths: &paths,
            root: dir.path(),
            distro: crate::cc::tests::fixture_distro(),
            datasource: None,
            logger: &mut logger,
        };
        assert!(handle(&mut args).is_err());
    }
}
