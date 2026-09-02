//! `cloud-init collect-logs` — gather logs and config into a tarball.
//!
//! Port of `cloudinit/cmd/devel/logs.py`.

use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

const DEFAULT_TARFILE: &str = "cloud-init.tar.gz";

/// `INSTALLER_APPORT_FILES` — collected regardless of `--redact-sensitive`.
const INSTALLER_FILES: [&str; 11] = [
    "/var/log/installer/ubuntu_desktop_installer.log",
    "/var/log/installer/subiquity-server-debug.log",
    "/var/log/installer/subiquity-client-debug.log",
    "/var/log/installer/curtin-install.log",
    "/var/log/installer/subiquity-curtin-install.conf",
    "/var/log/installer/curtin-install/subiquity-initial.conf",
    "/var/log/installer/curtin-install/subiquity-curthooks.conf",
    "/var/log/installer/curtin-install/subiquity-extract.conf",
    "/var/log/installer/curtin-install/subiquity-partitioning.conf",
    "/var/log/installer/curtin-error-logs.tar",
    "/var/log/installer/curtin-errors.tar",
];

/// `INSTALLER_APPORT_SENSITIVE_FILES`.
const INSTALLER_SENSITIVE_FILES: [&str; 3] = [
    "/var/log/installer/autoinstall-user-data",
    "/autoinstall.yaml",
    "/etc/cloud/cloud.cfg.d/99-installer.cfg",
];

const PROBE_DATA: &str = "/var/log/installer/block/probe-data.json";

const SENSITIVE_WARNING: &str = "WARNING:\n\
     Sensitive data may have been included in the collected logs.\n\
     Please review the contents of the tarball before sharing or\n\
     rerun with --redact-sensitive to redact sensitive data.";

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Be more verbose.
    #[arg(long, short = 'v', action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// The tarfile to create containing all collected logs. Default: cloud-init.tar.gz
    #[arg(long, short = 't', default_value = DEFAULT_TARFILE)]
    pub tarfile: String,

    /// DEPRECATED: This is default behavior and this flag does nothing
    #[arg(long = "include-userdata", short = 'u')]
    pub userdata: bool,

    /// Redact potentially sensitive data from logs.
    #[arg(long = "redact-sensitive", short = 'r')]
    pub redact_sensitive: bool,
}

pub fn run(args: &Args) -> u8 {
    if !ci_sys::is_root() {
        eprintln!("This command must be run as root.");
        return 1;
    }
    if args.userdata {
        eprintln!("The --include-userdata flag is deprecated and does nothing.");
    }

    let cfg = ci_config::read::fetch_base_config(None, ci_config::Limits::default())
        .unwrap_or_default();
    let paths = ci_core::Paths::from_config(&cfg);
    let options = Options {
        tarfile: Path::new(&args.tarfile).to_path_buf(),
        run_dir: paths.run_dir.clone(),
        cloud_dir: paths.cloud_dir.clone(),
        etc_cloud: PathBuf::from("/etc/cloud"),
        include_sensitive: !args.redact_sensitive,
        verbose: args.verbose > 0,
    };

    if let Err(e) = collect_logs(&options, &config_logfiles(&cfg)) {
        eprintln!("{e}");
        return 1;
    }
    eprintln!("Wrote {}", options.tarfile.display());
    if options.include_sensitive {
        eprintln!("{SENSITIVE_WARNING}");
    }
    0
}

struct Options {
    tarfile: PathBuf,
    run_dir: PathBuf,
    cloud_dir: PathBuf,
    etc_cloud: PathBuf,
    include_sensitive: bool,
    verbose: bool,
}

fn collect_logs(options: &Options, logfiles: &[PathBuf]) -> Result<(), String> {
    let tarfile = absolute(&options.tarfile);
    let dir_name = format!(
        "cloud-init-logs-{}",
        ci_core::time::format_iso_date(ci_core::time::now_epoch())
    );

    let tmp =
        ci_sys::path::TempDir::new(&options.run_dir, "cloudinit-").map_err(|e| {
            format!(
                "Cannot create a work directory in {}: {e}",
                options.run_dir.display()
            )
        })?;
    let log_dir = tmp.path().join(&dir_name);

    collect_version_info(&log_dir, options);
    collect_system_logs(&log_dir, options);
    collect_installer_logs(&log_dir, options);

    // Log files are root-only but the tarball is useless without them, and
    // upstream is careful to keep sensitive values out of them.
    for logfile in logfiles {
        collect_file(logfile, &mirror_dir(&log_dir, logfile), true, options);
    }
    for path in etc_cloud_files(&options.etc_cloud)
        .into_iter()
        .chain(var_lib_cloud_files(&options.cloud_dir))
        .chain(run_dir_files(&options.run_dir))
    {
        collect_file(
            &path,
            &mirror_dir(&log_dir, &path),
            options.include_sensitive,
            options,
        );
    }

    let output = ci_sys::subp::Subp::new([
        "tar".as_ref(),
        "czf".as_ref(),
        tarfile.as_os_str(),
        "-C".as_ref(),
        tmp.path().as_os_str(),
        dir_name.as_ref(),
    ])
    .run()
    .map_err(|e| format!("Cannot create {}: {e}", tarfile.display()))?;
    if !output.success() {
        return Err(format!("Cannot create {}", tarfile.display()));
    }
    Ok(())
}

/// `_collect_file()`: skip anything not world-readable unless sensitive data is wanted.
fn collect_file(
    path: &Path,
    out_dir: &Path,
    include_sensitive: bool,
    options: &Options,
) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    let world_readable = meta.permissions().mode() & 0o004 != 0;
    if !include_sensitive && !world_readable {
        return;
    }
    let Some(name) = path.file_name() else {
        return;
    };
    if std::fs::create_dir_all(out_dir).is_err() {
        return;
    }
    if std::fs::copy(path, out_dir.join(name)).is_ok() && options.verbose {
        eprintln!("collected file: {}", path.display());
    }
}

fn collect_version_info(log_dir: &Path, options: &Options) {
    command_to_file(
        &["cloud-init", "--version"],
        &log_dir.join("version"),
        options,
    );
    command_to_file(
        &["dpkg-query", "--show", "-f=${Version}\n", "cloud-init"],
        &log_dir.join("dpkg-version"),
        options,
    );
}

fn collect_system_logs(log_dir: &Path, options: &Options) {
    if options.include_sensitive {
        command_to_file(&["dmesg"], &log_dir.join("dmesg.txt"), options);
    }
    command_to_file(
        &["journalctl", "--boot=0", "-o", "short-precise"],
        &log_dir.join("journal.txt"),
        options,
    );
    command_to_file(
        &["journalctl", "--boot=-1", "-o", "short-precise"],
        &log_dir.join("journal-previous.txt"),
        options,
    );
}

fn collect_installer_logs(log_dir: &Path, options: &Options) {
    let mut files: Vec<&str> = INSTALLER_FILES.to_vec();
    files.push(PROBE_DATA);
    if options.include_sensitive {
        files.extend_from_slice(&INSTALLER_SENSITIVE_FILES);
    }
    for src in files {
        let src = Path::new(src);
        collect_file(src, &mirror_dir(log_dir, src), true, options);
    }
}

fn command_to_file(argv: &[&str], file_path: &Path, options: &Options) {
    let Some(parent) = file_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = match ci_sys::subp::run(argv.iter().copied()) {
        Ok(output) if output.success() => output.stdout_lossy().into_owned(),
        Ok(output) => String::from_utf8_lossy(&output.stderr).into_owned(),
        Err(e) => e.to_string(),
    };
    let _ = ci_sys::atomic::write_file(
        file_path,
        &body,
        ci_sys::atomic::WriteOptions::SECRET.volatile(),
    );
    if options.verbose {
        eprintln!("collected {}", argv.join(" "));
    }
}

/// `_get_etc_cloud()`. Upstream only excludes the direct children of `keys/` and
/// `templates/`; anything nested deeper is collected (docs/COMPAT.md B11).
fn etc_cloud_files(etc_cloud: &Path) -> Vec<PathBuf> {
    let excluded = [etc_cloud.join("keys"), etc_cloud.join("templates")];
    walk(etc_cloud)
        .into_iter()
        .filter(|path| {
            !excluded.iter().any(|dir| path.starts_with(dir))
                && path.file_name().is_none_or(|n| n != "99-installer.cfg")
        })
        .collect()
}

fn var_lib_cloud_files(cloud_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for sub in ["data", "handlers", "seed", "instance"] {
        out.extend(children(&cloud_dir.join(sub)));
    }
    out
}

fn run_dir_files(run_dir: &Path) -> Vec<PathBuf> {
    children(run_dir)
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for path in children(&current) {
            if path.is_dir() && !path.is_symlink() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

/// The tarball mirrors the absolute layout: `/etc/cloud/x` becomes `<log_dir>/etc/cloud/x`.
fn mirror_dir(log_dir: &Path, source: &Path) -> PathBuf {
    let parent = source.parent().unwrap_or(Path::new("/"));
    log_dir.join(parent.strip_prefix("/").unwrap_or(parent))
}

/// `util.get_config_logfiles()`: `def_log_file` plus any `output` redirect targets,
/// then every rotated sibling of those.
pub(crate) fn config_logfiles(cfg: &ci_config::Object) -> Vec<PathBuf> {
    let mut logs: Vec<String> = Vec::new();
    if let Some(path) = cfg.get("def_log_file").and_then(|v| v.as_str()) {
        logs.push(path.to_owned());
    }
    for fmt in output_targets(cfg) {
        if let Some(target) = redirect_target(&fmt) {
            logs.push(target);
        }
    }

    let mut all: Vec<PathBuf> = Vec::new();
    for log in &logs {
        all.push(PathBuf::from(log));
        all.extend(rotated_siblings(Path::new(log)));
    }
    all.sort();
    all.dedup();
    all
}

/// `util.get_output_cfg(cfg, None)` flattened to the strings it can yield.
fn output_targets(cfg: &ci_config::Object) -> Vec<String> {
    let Some(output) = cfg.get("output").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in output.values() {
        match value {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(items) => out.extend(
                items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .map(ToOwned::to_owned),
            ),
            serde_json::Value::Object(map) => out.extend(
                map.values()
                    .filter_map(|i| i.as_str())
                    .map(ToOwned::to_owned),
            ),
            _ => {}
        }
    }
    out
}

/// `re.match(r"(?P<type>\||>+)\s*(?P<target>.*)", fmt)` plus the `tee -a` special case.
fn redirect_target(fmt: &str) -> Option<String> {
    let rest = if let Some(rest) = fmt.strip_prefix('|') {
        rest
    } else if fmt.starts_with('>') {
        fmt.trim_start_matches('>')
    } else {
        return None;
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.as_slice() {
        [only] => Some((*only).to_owned()),
        ["tee", "-a", target, ..] => Some((*target).to_owned()),
        _ => None,
    }
}

fn rotated_siblings(logfile: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (logfile.parent(), logfile.file_name()) else {
        return Vec::new();
    };
    let Some(name) = name.to_str() else {
        return Vec::new();
    };
    children(dir)
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path != logfile
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(name))
        })
        .collect()
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
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
    use serde_json::json;

    fn object(value: &serde_json::Value) -> ci_config::Object {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn reads_the_default_log_file_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let cfg = object(&json!({"def_log_file": log.to_str().unwrap()}));
        assert_eq!(config_logfiles(&cfg), [log]);
    }

    #[test]
    fn understands_the_tee_output_form() {
        assert_eq!(
            redirect_target("| tee -a /var/log/cloud-init-output.log").unwrap(),
            "/var/log/cloud-init-output.log"
        );
        assert_eq!(
            redirect_target("> /var/log/cloud-init.out").unwrap(),
            "/var/log/cloud-init.out"
        );
        assert_eq!(
            redirect_target(">> /var/log/cloud-final.out").unwrap(),
            "/var/log/cloud-final.out"
        );
        assert_eq!(redirect_target("tee -a /var/log/x.log"), None);
        assert_eq!(redirect_target("| tee -a"), None);
    }

    #[test]
    fn collects_every_output_stage() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();
        let cfg = object(&json!({
            "output": {
                "init": {"output": format!("> {}", at("a.out")),
                         "error": format!("> {}", at("a.err"))},
                "final": [format!("> {}", at("b.out"))],
                "all": format!("| tee -a {}", at("c.log"))
            }
        }));
        assert_eq!(
            config_logfiles(&cfg),
            [
                dir.path().join("a.err"),
                dir.path().join("a.out"),
                dir.path().join("b.out"),
                dir.path().join("c.log"),
            ]
        );
    }

    #[test]
    fn mirrors_the_absolute_path_under_the_log_dir() {
        assert_eq!(
            mirror_dir(
                Path::new("/tmp/x/cloud-init-logs-2026-09-01"),
                Path::new("/etc/cloud/cloud.cfg")
            ),
            PathBuf::from("/tmp/x/cloud-init-logs-2026-09-01/etc/cloud")
        );
    }

    #[test]
    fn excludes_keys_and_templates_at_every_depth() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        std::fs::create_dir_all(etc.join("keys/nested")).unwrap();
        std::fs::create_dir_all(etc.join("templates")).unwrap();
        std::fs::create_dir_all(etc.join("cloud.cfg.d")).unwrap();
        std::fs::write(etc.join("cloud.cfg"), "").unwrap();
        std::fs::write(etc.join("keys/nested/secret.pem"), "").unwrap();
        std::fs::write(etc.join("templates/hosts.tmpl"), "").unwrap();
        std::fs::write(etc.join("cloud.cfg.d/99-installer.cfg"), "").unwrap();
        std::fs::write(etc.join("cloud.cfg.d/05_logging.cfg"), "").unwrap();

        let got = etc_cloud_files(etc);
        assert!(got.contains(&etc.join("cloud.cfg")));
        assert!(got.contains(&etc.join("cloud.cfg.d/05_logging.cfg")));
        assert!(!got.iter().any(|p| p.starts_with(etc.join("keys"))));
        assert!(!got.iter().any(|p| p.starts_with(etc.join("templates"))));
        assert!(!got.contains(&etc.join("cloud.cfg.d/99-installer.cfg")));
    }

    #[test]
    fn skips_files_that_are_not_world_readable_when_redacting() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "shh").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let out = dir.path().join("out");
        let options = Options {
            tarfile: PathBuf::new(),
            run_dir: PathBuf::new(),
            cloud_dir: PathBuf::new(),
            etc_cloud: PathBuf::new(),
            include_sensitive: false,
            verbose: false,
        };

        collect_file(&secret, &out, false, &options);
        assert!(!out.join("secret").exists());
        collect_file(&secret, &out, true, &options);
        assert!(out.join("secret").exists());
    }

    #[test]
    fn finds_rotated_log_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        std::fs::write(&log, "").unwrap();
        std::fs::write(dir.path().join("cloud-init.log.1"), "").unwrap();
        std::fs::write(dir.path().join("other.log"), "").unwrap();
        assert_eq!(
            rotated_siblings(&log),
            [dir.path().join("cloud-init.log.1")]
        );
    }
}
