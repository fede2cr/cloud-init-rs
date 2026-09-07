//! Port of `cloudinit/config/cc_bootcmd.py`.
//!
//! The counterpart to `runcmd`: this one really does run the commands, in the
//! init stage, before the network is up and before any package is installed.
//! It is `once-per-boot`, not `once-per-instance`, so anything here runs on
//! every reboot for the life of the instance — which is the module's whole
//! reason to exist and also its sharpest edge.

use std::time::Duration;

use ci_sys::atomic::{self, WriteOptions};
use ci_sys::subp::Subp;

use super::{shellify, Args};

const SOURCE: &str = "cc_bootcmd.py";

/// `subp.subp` passes no timeout at all here, so upstream will wait forever on
/// a hung `bootcmd`. Blocking the boot indefinitely on tenant data is not a
/// behaviour worth reproducing; an hour is far past any legitimate use and far
/// short of "never" (COMPAT.md deviation 105).
const TIMEOUT: Duration = Duration::from_secs(3600);

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let Some(commands) = args.cfg.get("bootcmd").cloned() else {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!("Skipping module named {name}, no 'bootcmd' key in configuration"),
        );
        return Ok(());
    };

    let (content, made) = shellify(&commands).map_err(|err| {
        args.warning(SOURCE, &format!("Failed to shellify bootcmd: {err}"));
        err
    })?;
    args.debug("util.py", &format!("Shellified {made} commands."));

    // The script is written to a private directory rather than a temp file
    // with a predictable name: it runs as root, so anything that can swap the
    // file between write and exec chooses what root runs.
    let dir = ci_sys::path::TempDir::new(std::env::temp_dir(), "cloud-init-bootcmd")
        .map_err(|err| format!("Failed to run bootcmd module {}: {err}", args.name))?;
    let script = dir.path().join("bootcmd.sh");
    atomic::write_file(&script, content, WriteOptions::mode(0o700))
        .map_err(|err| format!("Failed to run bootcmd module {}: {err}", args.name))?;

    let mut cmd = Subp::new(["/bin/sh".as_ref(), script.as_os_str()])
        .inherit_env()
        .timeout(Some(TIMEOUT));
    if let Some(ds) = args.datasource {
        cmd = cmd.env("INSTANCE_ID", ds.instance_id);
    }
    let status = cmd
        .passthrough()
        .map_err(|err| format!("Failed to run bootcmd module {}: {err}", args.name))?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "Failed to run bootcmd module {}: /bin/sh exited {}",
        args.name,
        status
            .code()
            .map_or_else(|| "on a signal".to_owned(), |c| c.to_string())
    ))
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
        let paths = ci_core::Paths::default();
        let mut logger = Logger::silent();
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "bootcmd",
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

    #[test]
    fn a_config_without_the_key_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), &json!({})), Ok(()));
    }

    #[test]
    fn the_commands_run_in_order_and_see_the_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        run(
            dir.path(),
            &json!({"bootcmd": [
                ["sh", "-c", format!("echo one > '{}'", out.display())],
                format!("printf '%s\\n' \"$INSTANCE_ID\" >> '{}'", out.display()),
            ]}),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "one\ni-test\n");
    }

    #[test]
    fn a_failing_command_fails_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), &json!({"bootcmd": ["exit 3"]})).unwrap_err();
        assert!(err.contains("exited 3"), "{err}");
    }

    #[test]
    fn an_unshellifiable_bootcmd_never_reaches_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(dir.path(), &json!({"bootcmd": {"a": 1}})).unwrap_err();
        assert!(err.contains("Expected list or tuple"), "{err}");
    }
}
