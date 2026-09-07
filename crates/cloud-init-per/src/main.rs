//! `cloud-init-per` — port of `tools/cloud-init-per`.
//!
//! Runs a command at most once per boot, per instance, or unconditionally,
//! recording the outcome in a semaphore file. The semaphore layout and the
//! `<exit code>\t<epoch>` file format are consumed by user-provided `bootcmd`
//! scripts, so they are part of the compatibility contract.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const DATA_PRE: &str = "/var/lib/cloud/sem/bootper";
const INST_PRE: &str = "/var/lib/cloud/instance/sem/bootper";

const USAGE: &str = "\
Usage: cloud-init-per frequency name cmd [ arg1 [ arg2 [ ... ] ]
   run cmd with arguments provided.

   This utility can make it easier to use boothooks or bootcmd
   on a per \"once\" or \"always\" basis.

   If frequency is:
      * once: run only once (do not re-run for new instance-id)
      * instance: run only the first boot for a given instance-id
      * always: run every boot

";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(run(&args))
}

fn run(args: &[String]) -> u8 {
    if matches!(args.first().map(String::as_str), Some("-h" | "--help")) {
        print!("{USAGE}");
        return 0;
    }
    let [freq, raw_name, cmd @ ..] = args else {
        eprint!("{USAGE}");
        return 1;
    };
    if cmd.is_empty() {
        eprint!("{USAGE}");
        return 1;
    }

    // Dashes are normalised to underscores so that semaphore names are stable
    // regardless of how the caller spelled them.
    let name = raw_name.replace('-', "_");

    if name.contains('/') {
        return fail("name cannot contain a /");
    }
    if !ci_sys::is_root() {
        return fail("must be root");
    }

    let Some(sem) = sem_path(freq.as_str(), &name) else {
        eprint!("{USAGE}");
        return fail(&format!("invalid frequency: {freq}"));
    };

    if let Some(parent) = sem.parent() {
        if !parent.is_dir() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return fail(&format!(
                    "failed to make directory for {}: {e}",
                    sem.display()
                ));
            }
        }
    }

    migrate_legacy_sem(&sem);

    if freq.as_str() != "always" && sem.exists() {
        return 0;
    }

    let ret = spawn(cmd);

    let record = format!("{ret}\t{}\n", ci_core::time::now_epoch());
    if ci_sys::write_file(&sem, record.as_bytes(), ci_sys::WriteOptions::PUBLIC)
        .is_err()
    {
        return fail(&format!("failed to write to {}", sem.display()));
    }
    ret
}

fn sem_path(freq: &str, name: &str) -> Option<PathBuf> {
    let prefix = match freq {
        "once" | "always" => DATA_PRE,
        "instance" => INST_PRE,
        _ => return None,
    };
    Some(PathBuf::from(format!("{prefix}.{name}.{freq}")))
}

/// Rename dash-named semaphores from older releases, never clobbering an
/// existing file (semaphores may have been created outside cloud-init).
fn migrate_legacy_sem(sem: &Path) {
    let legacy = PathBuf::from(sem.to_string_lossy().replace('_', "-"));
    if legacy != sem && legacy.exists() && !sem.exists() {
        let _ = std::fs::rename(&legacy, sem);
    }
}

/// Run the command with inherited stdio, mapping signals the way a shell does.
fn spawn(cmd: &[String]) -> u8 {
    let Some((program, rest)) = cmd.split_first() else {
        return 127;
    };
    match Command::new(program).args(rest).status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                u8::try_from(code).unwrap_or(1)
            } else {
                use std::os::unix::process::ExitStatusExt;
                status
                    .signal()
                    .and_then(|s| u8::try_from(128 + s).ok())
                    .unwrap_or(1)
            }
        }
        Err(e) => {
            eprintln!("cloud-init-per: {program}: {e}");
            127
        }
    }
}

fn fail(message: &str) -> u8 {
    eprintln!("{message}");
    1
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
    fn semaphore_paths_follow_frequency() {
        assert_eq!(
            sem_path("once", "my_task").unwrap(),
            PathBuf::from("/var/lib/cloud/sem/bootper.my_task.once")
        );
        assert_eq!(
            sem_path("instance", "my_task").unwrap(),
            PathBuf::from("/var/lib/cloud/instance/sem/bootper.my_task.instance")
        );
        assert!(sem_path("hourly", "my_task").is_none());
    }

    #[test]
    fn legacy_dash_semaphores_are_migrated_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let sem = dir.path().join("bootper.my_task.once");
        let legacy = dir.path().join("bootper.my-task.once");

        std::fs::write(&legacy, "0\t1\n").unwrap();
        migrate_legacy_sem(&sem);
        assert!(sem.exists() && !legacy.exists());

        std::fs::write(&legacy, "9\t9\n").unwrap();
        migrate_legacy_sem(&sem);
        assert_eq!(std::fs::read_to_string(&sem).unwrap(), "0\t1\n");
        assert!(legacy.exists());
    }
}
