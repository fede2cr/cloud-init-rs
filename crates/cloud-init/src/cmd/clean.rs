//! `cloud-init clean` — remove logs, configs and artifacts so cloud-init re-runs.
//!
//! Ported from `cloudinit/cmd/clean.py`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ci_core::paths::Paths;

const ETC_MACHINE_ID: &str = "/etc/machine-id";
const FSTAB_PATH: &str = "/etc/fstab";
/// `cc_mounts.MNT_COMMENT`.
const MNT_COMMENT: &str = "comment=cloudconfig";

/// `GEN_NET_CONFIG_FILES`, with `CLOUDINIT_NETPLAN_FILE` expanded.
const GEN_NET_CONFIG_FILES: &[&str] = &[
    "/etc/netplan/50-cloud-init.yaml",
    "/etc/NetworkManager/conf.d/99-cloud-init.conf",
    "/etc/NetworkManager/conf.d/30-cloud-init-ip6-addr-gen-mode.conf",
    "/etc/NetworkManager/system-connections/cloud-init-*.nmconnection",
    "/etc/systemd/network/10-cloud-init-*.network",
    "/etc/network/interfaces.d/50-cloud-init.cfg",
];

const GEN_SSH_CONFIG_FILES: &[&str] = &["/etc/ssh/sshd_config.d/50-cloud-init.conf"];

const CONFIG_CHOICES: [&str; 5] =
    ["all", "ssh_config", "network", "datasource", "fstab"];

/// The four bools are upstream's four independent flags; a state machine would
/// only obscure the mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Remove cloud-init logs.
    #[arg(short = 'l', long = "logs")]
    remove_logs: bool,

    /// Set /etc/machine-id to 'uninitialized\n' for golden image creation.
    #[arg(long = "machine-id")]
    machine_id: bool,

    /// Reboot system after logs are cleaned so cloud-init re-runs.
    #[arg(short = 'r', long = "reboot")]
    reboot: bool,

    /// Remove cloud-init seed directory /var/lib/cloud/seed.
    #[arg(short = 's', long = "seed")]
    remove_seed: bool,

    /// Remove cloud-init generated config files of a certain type.
    #[arg(
        short = 'c',
        long = "configs",
        num_args = 1..,
        value_parser = CONFIG_CHOICES,
    )]
    remove_config: Vec<String>,
}

pub fn run(args: &Args) -> u8 {
    let cfg = ci_config::read::fetch_base_config(None, ci_config::Limits::default())
        .unwrap_or_default();
    let paths = Paths::from_config(&cfg);
    let mut exit_code = remove_artifacts(&cfg, &paths, args);

    if args.machine_id {
        if let Err(e) = set_machine_id() {
            error(&format!("Could not write {ETC_MACHINE_ID}: {e}"));
            exit_code = 1;
        }
    }
    if exit_code == 0 && args.reboot {
        // `distro.shutdown_command(mode="reboot", delay="now", message=None)`.
        let cmd = ["shutdown", "-r", "now"];
        if let Err(e) = ci_sys::subp::Subp::new(cmd).run() {
            error(&format!(
                "Could not reboot this system using \"{}\": {e}",
                cmd.join("\", \"")
            ));
            exit_code = 1;
        }
    }
    exit_code
}

fn remove_artifacts(cfg: &ci_config::Object, paths: &Paths, args: &Args) -> u8 {
    // Upstream's `del_file` swallows only FileNotFoundError, so a permission
    // error here escapes as a traceback (COMPAT.md B14). Report it the way the
    // cloud_dir loop below already reports the identical failure.
    if args.remove_logs {
        for log_file in super::collect_logs::config_logfiles(cfg) {
            if should_remove_log_file(&log_file) {
                if let Err(e) = del_file(&log_file) {
                    error(&format!("Could not remove {}: {}", log_file.display(), e));
                    return 1;
                }
            }
        }
    }
    if wants(args, "network") {
        for pattern in GEN_NET_CONFIG_FILES {
            for path in glob(pattern) {
                if let Err(e) = del_file(&path) {
                    error(&format!("Could not remove {}: {}", path.display(), e));
                    return 1;
                }
            }
        }
    }
    if wants(args, "ssh_config") {
        for path in GEN_SSH_CONFIG_FILES {
            if let Err(e) = del_file(Path::new(path)) {
                error(&format!("Could not remove {path}: {e}"));
                return 1;
            }
        }
    }
    if wants(args, "fstab") {
        cleanup_fstab(Path::new(FSTAB_PATH));
    }

    if !paths.cloud_dir.is_dir() {
        multi_log("Artifacts already cleaned.");
        return 0;
    }

    let seed_path = paths.seed_dir();
    for path in glob(&format!("{}/*", paths.cloud_dir.display())) {
        if path == seed_path && !args.remove_seed {
            continue;
        }
        if let Err(e) = remove_entry(&path) {
            error(&format!("Could not remove {}: {}", path.display(), e));
            return 1;
        }
    }
    if let Err(e) = runparts(Path::new(ci_config::builtin::CLEAN_RUNPARTS_DIR)) {
        error(&format!(
            "Failure during run-parts of {}: {e}",
            ci_config::builtin::CLEAN_RUNPARTS_DIR
        ));
        return 1;
    }
    0
}

fn wants(args: &Args, kind: &str) -> bool {
    args.remove_config.iter().any(|c| c == "all" || c == kind)
}

/// Directories go recursively, but a symlink to one is only unlinked.
fn remove_entry(path: &Path) -> io::Result<()> {
    if path.is_dir() && !path.is_symlink() {
        match fs::remove_dir_all(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    } else {
        match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// `del_file`: a missing path is not an error.
fn del_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Avoid tracebacks from attempting to remove device files.
fn should_remove_log_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        match fs::metadata(path) {
            Ok(meta) => {
                let kind = meta.file_type();
                !kind.is_block_device() && !kind.is_char_device()
            }
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

/// `glob.glob`: a single `*` in the final component, never matching dotfiles.
fn glob(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    let (Some(parent), Some(name)) =
        (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Vec::new();
    };
    let Some((prefix, suffix)) = name.split_once('*') else {
        return if path.exists() {
            vec![path.to_owned()]
        } else {
            Vec::new()
        };
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            !name.starts_with('.')
                && name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        })
        .map(|entry| entry.path())
        .collect();
    out.sort();
    out
}

/// `cc_mounts.cleanup_fstab`: drop every line mentioning the cloud-init marker.
fn cleanup_fstab(path: &Path) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let kept: Vec<&str> = content
        .split_inclusive('\n')
        .filter(|line| !line.contains(MNT_COMMENT))
        .collect();
    if kept.len() != content.split_inclusive('\n').count() {
        let _ = fs::write(path, kept.concat());
    }
}

/// `subp.runparts`: run every executable in `dir`, in name order.
fn runparts(dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    let mut attempted = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for path in paths {
        if !is_executable(&path) {
            continue;
        }
        attempted += 1;
        if ci_sys::subp::Subp::new([&path]).run().is_err() {
            failed.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into(),
            );
        }
    }
    if !failed.is_empty() && attempted > 0 {
        return Err(format!(
            "Runparts: {} failures ({}) in {attempted} attempted commands",
            failed.len(),
            failed.join(",")
        ));
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn set_machine_id() -> io::Result<()> {
    if ci_core::status::uses_systemd() {
        // Systemd v237 and later will create a new machine-id on next boot.
        ci_sys::write_file(
            Path::new(ETC_MACHINE_ID),
            b"uninitialized\n",
            ci_sys::WriteOptions {
                mode: 0o444,
                durable: true,
            },
        )
    } else {
        // Non-systemd like FreeBSD regen machine-id when the file is absent.
        del_file(Path::new(ETC_MACHINE_ID))
    }
}

/// `log_util.error`, whose default format is `"Error:\n{}"`.
fn error(message: &str) {
    eprintln!("Error:\n{message}");
}

/// `log_util.multi_log`: stderr, then the console, falling back to stdout.
fn multi_log(text: &str) {
    eprint!("{text}");
    let console = Path::new("/dev/console");
    let mut written = false;
    if console.exists() {
        match fs::write(console, text.as_bytes()) {
            Ok(()) => written = true,
            Err(_) => println!("Failed to write to /dev/console"),
        }
    }
    if !written {
        print!("{text}");
    }
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

    fn args(configs: &[&str]) -> Args {
        Args {
            remove_logs: false,
            machine_id: false,
            reboot: false,
            remove_seed: false,
            remove_config: configs.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn all_selects_every_config_type() {
        let a = args(&["all"]);
        for kind in ["network", "ssh_config", "fstab", "datasource"] {
            assert!(wants(&a, kind), "{kind}");
        }
    }

    #[test]
    fn a_named_type_selects_only_itself() {
        let a = args(&["network"]);
        assert!(wants(&a, "network"));
        assert!(!wants(&a, "ssh_config"));
    }

    #[test]
    fn no_configs_selects_nothing() {
        let a = args(&[]);
        assert!(!wants(&a, "network"));
    }

    #[test]
    fn glob_matches_a_star_in_the_final_component() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "cloud-init-eth0.nmconnection",
            "other.nmconnection",
            ".hidden",
        ] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        let pattern = format!("{}/cloud-init-*.nmconnection", dir.path().display());
        let found = glob(&pattern);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("cloud-init-eth0.nmconnection"));
    }

    #[test]
    fn glob_never_matches_dotfiles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".secret"), b"").unwrap();
        fs::write(dir.path().join("visible"), b"").unwrap();
        let found = glob(&format!("{}/*", dir.path().display()));
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("visible"));
    }

    #[test]
    fn cleanup_fstab_drops_only_marked_lines() {
        let dir = tempfile::tempdir().unwrap();
        let fstab = dir.path().join("fstab");
        fs::write(
            &fstab,
            "/dev/sda / ext4 defaults 0 1\n/dev/sdb /mnt auto comment=cloudconfig 0 2\n",
        )
        .unwrap();
        cleanup_fstab(&fstab);
        assert_eq!(
            fs::read_to_string(&fstab).unwrap(),
            "/dev/sda / ext4 defaults 0 1\n"
        );
    }

    #[test]
    fn cleanup_fstab_leaves_an_unmarked_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let fstab = dir.path().join("fstab");
        fs::write(&fstab, "/dev/sda / ext4 defaults 0 1\n").unwrap();
        cleanup_fstab(&fstab);
        assert_eq!(
            fs::read_to_string(&fstab).unwrap(),
            "/dev/sda / ext4 defaults 0 1\n"
        );
    }

    #[test]
    fn removing_a_symlinked_directory_only_unlinks_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_entry(&link).unwrap();
        assert!(!link.exists());
        assert!(target.join("keep").exists());
    }

    #[test]
    fn removing_a_missing_path_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remove_entry(&dir.path().join("nope")).is_ok());
    }

    #[test]
    fn runparts_on_a_missing_directory_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        assert!(runparts(&dir.path().join("absent")).is_ok());
    }

    #[test]
    fn a_device_file_is_never_removed() {
        assert!(!should_remove_log_file(Path::new("/dev/null")));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        fs::write(&log, b"x").unwrap();
        assert!(should_remove_log_file(&log));
    }
}
