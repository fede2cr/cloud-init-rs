//! Port of `cloudinit/config/cc_ca_certs.py`.
//!
//! Installs tenant-supplied CA certificates into the system trust store, and
//! optionally throws the shipped ones away first. Both halves are worth
//! treating carefully: the first decides who this machine will believe, and
//! the second can leave it believing nobody.
//!
//! Split into [`plan`] and [`run`] like the other modules that shell out, so
//! the decision can be compared against upstream without a differential
//! harness running `update-ca-certificates` on the machine it is testing.

use std::path::Path;

use ci_config::{Object, Value};
use ci_log::Logger;

use super::Args;

const SOURCE: &str = "cc_ca_certs.py";

/// The header `disable_system_ca_certs` inserts before the first line it
/// comments out, and recognises on a second pass so it is not inserted twice.
const HEADER_COMMENT: &str =
    "# Modified by cloud-init to deselect certs due to user-data";

/// One distro's entry in upstream's `DEFAULT_CONFIG`/`DISTRO_OVERRIDES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistroCfg {
    /// The trust store to empty when `remove_defaults` is set. `None` means
    /// the distro has no separate one, and makes `remove_default_ca_certs` a
    /// no-op.
    pub ca_cert_path: Option<&'static str>,
    /// Where a tenant certificate is written.
    pub ca_cert_local_path: &'static str,
    /// Template for the filename; `{cert_index}` counts from 1.
    pub ca_cert_filename: &'static str,
    /// The `ca-certificates.conf`-style selection file, if the distro has one.
    pub ca_cert_config: Option<&'static str>,
    pub ca_cert_update_cmd: &'static [&'static str],
}

impl DistroCfg {
    /// `os.path.join(ca_cert_local_path, ca_cert_filename)`, still holding the
    /// `{cert_index}` placeholder.
    #[must_use]
    pub fn full_path(&self) -> String {
        join(self.ca_cert_local_path, self.ca_cert_filename)
    }
}

const DEFAULT_CONFIG: DistroCfg = DistroCfg {
    ca_cert_path: None,
    ca_cert_local_path: "/usr/local/share/ca-certificates/",
    ca_cert_filename: "cloud-init-ca-cert-{cert_index}.crt",
    ca_cert_config: Some("/etc/ca-certificates.conf"),
    ca_cert_update_cmd: &["update-ca-certificates"],
};

const AOSC: DistroCfg = DistroCfg {
    ca_cert_path: Some("/etc/ssl/certs/"),
    ca_cert_local_path: "/etc/ssl/certs/",
    ca_cert_filename: "cloud-init-ca-cert-{cert_index}.pem",
    ca_cert_config: Some("/etc/ca-certificates/conf.d/cloud-init.conf"),
    ca_cert_update_cmd: &["update-ca-bundle"],
};

const RHEL: DistroCfg = DistroCfg {
    ca_cert_path: Some("/etc/pki/ca-trust/"),
    ca_cert_local_path: "/usr/share/pki/ca-trust-source/",
    ca_cert_filename: "anchors/cloud-init-ca-cert-{cert_index}.crt",
    ca_cert_config: None,
    ca_cert_update_cmd: &["update-ca-trust"],
};

const OPENSUSE: DistroCfg = DistroCfg {
    ca_cert_path: Some("/etc/pki/trust/"),
    ca_cert_local_path: "/usr/share/pki/trust/",
    ca_cert_filename: "anchors/cloud-init-ca-cert-{cert_index}.crt",
    ca_cert_config: None,
    ca_cert_update_cmd: &["update-ca-certificates"],
};

const PHOTON: DistroCfg = DistroCfg {
    ca_cert_path: Some("/etc/pki/tls/certs/"),
    ca_cert_local_path: "/etc/ssl/certs/",
    ca_cert_filename: "cloud-init-ca-cert-{cert_index}.pem",
    ca_cert_config: None,
    ca_cert_update_cmd: &["rehash_ca_certificates.sh"],
};

/// `_distro_ca_certs_configs(distro_name)`.
///
/// `fedora` and `rhel` are the same table entry upstream; the aliases below
/// are the loops that copy `rhel`'s and `opensuse`'s dicts under further
/// names. An unknown distro gets the Debian-shaped default.
#[must_use]
pub fn distro_config(distro_name: &str) -> DistroCfg {
    match distro_name {
        "aosc" => AOSC,
        "fedora" | "rhel" | "almalinux" | "centos" | "cloudlinux" | "rocky" => RHEL,
        "opensuse"
        | "opensuse-microos"
        | "opensuse-tumbleweed"
        | "opensuse-leap"
        | "sle_hpc"
        | "sle-micro"
        | "sles" => OPENSUSE,
        "photon" => PHOTON,
        _ => DEFAULT_CONFIG,
    }
}

/// One action the module decided on, in the order upstream performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `util.delete_dir_contents` — the directory survives, everything in it
    /// does not.
    RemoveDirContents {
        path: String,
    },
    /// Prefix every enabled entry of the selection file with `!`.
    DisableSystemCaCerts {
        path: String,
    },
    /// `debconf-set-selections -`, so a later `dpkg-reconfigure` does not put
    /// the shipped certificates back.
    DebconfSetSelections {
        data: String,
    },
    WriteCert {
        path: String,
        contents: String,
    },
    UpdateCaCerts {
        argv: Vec<String>,
    },
}

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let steps = plan(args.name, args.cfg, args.distro.name, args.logger)?;
    run(&steps, args.root, args.logger)
}

/// Everything `handle` decides before it touches the machine.
///
/// An empty plan means the module was skipped; any other outcome always ends
/// with [`Step::UpdateCaCerts`], because upstream refreshes the trust store
/// whether or not it changed anything.
///
/// # Errors
/// Returns upstream's `TypeError` message when `ca_certs` is not a mapping.
pub fn plan(
    name: &str,
    cfg: &Object,
    distro_name: &str,
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let has_dashed = cfg.contains_key("ca-certs");
    if has_dashed {
        deprecate(log, "Key 'ca-certs'", "Use 'ca_certs' instead.");
    } else if !cfg.contains_key("ca_certs") {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, no 'ca_certs' key in configuration"
            ),
        );
        return Ok(Vec::new());
    }
    if has_dashed && cfg.contains_key("ca_certs") {
        log.warning(
            SOURCE,
            "Found both ca-certs (deprecated) and ca_certs config keys. Ignoring ca-certs.",
        );
    }

    let selected = cfg.get("ca_certs").or_else(|| cfg.get("ca-certs"));
    let Some(ca_cert_cfg) = selected.and_then(Value::as_object) else {
        // Upstream's message is a plain string, so the braces reach the log
        // verbatim rather than naming the value (bug B80).
        return Err("unexpected type: {ca_cert_cfg}".to_owned());
    };

    let distro_cfg = distro_config(distro_name);
    let mut steps = Vec::new();

    if ca_cert_cfg.contains_key("remove-defaults") {
        deprecate(
            log,
            "Key 'remove-defaults'",
            "Use 'remove_defaults' instead.",
        );
    }
    let remove = ca_cert_cfg
        .get("remove_defaults")
        .or_else(|| ca_cert_cfg.get("remove-defaults"));
    if remove.is_some_and(ci_config::option::py_truthy) {
        log.debug(SOURCE, "Disabling/removing default certificates");
        steps.extend(disable_default_ca_certs(distro_name, &distro_cfg, log));
    }

    if ca_cert_cfg.contains_key("trusted") {
        let trusted = cfg_option_list(ca_cert_cfg, "trusted");
        if !trusted.is_empty() {
            log.debug(SOURCE, &format!("Adding {} certificates", trusted.len()));
            let template = distro_cfg.full_path();
            for (index, cert) in trusted.iter().enumerate() {
                steps.push(Step::WriteCert {
                    path: template.replace("{cert_index}", &(index + 1).to_string()),
                    contents: super::py_str(cert),
                });
            }
        }
    }

    log.debug(SOURCE, "Updating certificates");
    steps.push(Step::UpdateCaCerts {
        argv: distro_cfg
            .ca_cert_update_cmd
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect(),
    });
    Ok(steps)
}

/// `disable_default_ca_certs`.
///
/// The distro lists are upstream's and are *not* the families: `centos` and
/// `rocky` share `rhel`'s paths but are absent from both arms here, so
/// `remove_defaults` silently does nothing on them.
fn disable_default_ca_certs(
    distro_name: &str,
    distro_cfg: &DistroCfg,
    log: &mut Logger,
) -> Vec<Step> {
    if matches!(distro_name, "rhel" | "photon") {
        let Some(path) = distro_cfg.ca_cert_path else {
            return Vec::new();
        };
        log.debug(SOURCE, "Deleting system CA certificates");
        return vec![
            Step::RemoveDirContents {
                path: path.to_owned(),
            },
            Step::RemoveDirContents {
                path: distro_cfg.ca_cert_local_path.to_owned(),
            },
        ];
    }
    if !matches!(
        distro_name,
        "alpine" | "aosc" | "debian" | "raspberry-pi-os" | "ubuntu"
    ) {
        return Vec::new();
    }

    let mut steps = Vec::new();
    if let Some(path) = distro_cfg.ca_cert_config {
        steps.push(Step::DisableSystemCaCerts {
            path: path.to_owned(),
        });
    }
    if matches!(distro_name, "debian" | "raspberry-pi-os" | "ubuntu") {
        steps.push(Step::DebconfSetSelections {
            data: "ca-certificates ca-certificates/trust_new_crts select no".to_owned(),
        });
    }
    steps
}

/// Carry out a plan under `root`.
///
/// # Errors
/// The first step that fails stops the module, as upstream's uncaught
/// exceptions do.
pub fn run(steps: &[Step], root: &Path, log: &mut Logger) -> Result<(), String> {
    // The two steps that shell out change the machine's trust store, so they
    // are skipped outright when the module is pointed at a fixture root.
    let live = root == Path::new("/");
    let rooted = |path: &str| super::rooted(root, path);
    for step in steps {
        match step {
            Step::RemoveDirContents { path } => {
                delete_dir_contents(&rooted(path))?;
            }
            Step::DisableSystemCaCerts { path } => {
                disable_system_ca_certs(&rooted(path))?;
            }
            Step::DebconfSetSelections { data } if live => {
                ci_sys::subp::Subp::new(["debconf-set-selections", "-"])
                    .stdin(data.clone())
                    .check()
                    .map(|_| ())
                    .map_err(|error| error.to_string())?;
            }
            Step::WriteCert { path, contents } => {
                write_cert(&rooted(path), contents)?;
            }
            Step::UpdateCaCerts { argv } if live => {
                log.debug(SOURCE, &format!("Running {}", argv.join(" ")));
                ci_sys::subp::Subp::new(argv)
                    .passthrough()
                    .map_err(|error| error.to_string())
                    .and_then(|status| {
                        if status.success() {
                            Ok(())
                        } else {
                            Err(format!("{} failed: {status}", argv.join(" ")))
                        }
                    })?;
            }
            Step::DebconfSetSelections { .. } | Step::UpdateCaCerts { .. } => {}
        }
    }
    Ok(())
}

/// `disable_system_ca_certs`, including its two early exits: a file that is
/// not there at all, and one that is there but empty.
fn disable_system_ca_certs(path: &Path) -> Result<(), String> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() == 0 {
        return Ok(());
    }
    let original = std::fs::read_to_string(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    super::write_file(path, deselect(&original).as_bytes())
}

/// The rewrite itself: every enabled entry gains a `!`, and the first one to
/// do so gets the header comment above it.
///
/// Comments, blank lines and already-disabled entries are copied through, so
/// running it twice is a no-op.
#[must_use]
pub fn deselect(original: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut added_header = false;
    for line in ci_core::pystr::split_lines(original) {
        if line == HEADER_COMMENT {
            added_header = true;
            lines.push(line.to_owned());
        } else if line.is_empty() || line.starts_with(['#', '!']) {
            lines.push(line.to_owned());
        } else {
            if !added_header {
                lines.push(HEADER_COMMENT.to_owned());
                added_header = true;
            }
            lines.push(format!("!{line}"));
        }
    }
    lines.join("\n") + "\n"
}

fn write_cert(path: &Path, contents: &str) -> Result<(), String> {
    super::write_file(path, contents.as_bytes())
}

/// `util.delete_dir_contents`. A missing directory is `os.listdir`'s
/// `FileNotFoundError`, which upstream does not catch.
fn delete_dir_contents(path: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", path.display()))?;
        let child = entry.path();
        let removed = if child.is_dir() && !child.is_symlink() {
            std::fs::remove_dir_all(&child)
        } else {
            std::fs::remove_file(&child)
        };
        removed.map_err(|error| format!("{}: {error}", child.display()))?;
    }
    Ok(())
}

/// `util.get_cfg_option_list`, stopping short of the stringification upstream
/// defers to `add_ca_certs`.
///
/// A list is taken element by element and each element is `str()`-ed later; a
/// scalar is stringified *now* and becomes the single element. The difference
/// is invisible for strings and visible for everything else.
fn cfg_option_list(map: &Object, key: &str) -> Vec<Value> {
    match map.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![Value::from(super::py_str(other))],
    }
}

/// `lifecycle.deprecate(deprecated=.., deprecated_version="22.1", ..)`.
fn deprecate(log: &mut Logger, deprecated: &str, extra: &str) {
    let message = format!(
        "{deprecated} is deprecated in 22.1 and scheduled to be removed in 27.1. {extra}"
    );
    log.log(ci_log::Level::Deprecated, "lifecycle.py", &message);
}

/// `os.path.join` for the one shape this module uses.
fn join(directory: &str, name: &str) -> String {
    if name.starts_with('/') {
        return name.to_owned();
    }
    if directory.ends_with('/') {
        return format!("{directory}{name}");
    }
    format!("{directory}/{name}")
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

    fn cfg(json: &str) -> Object {
        serde_json::from_str(json).unwrap()
    }

    fn plan_for(json: &str, distro: &str) -> Result<Vec<Step>, String> {
        plan("cc_ca_certs", &cfg(json), distro, &mut Logger::silent())
    }

    #[test]
    fn an_absent_key_skips_the_module_entirely() {
        assert_eq!(plan_for("{}", "ubuntu").unwrap(), Vec::new());
    }

    #[test]
    fn a_present_but_empty_mapping_still_refreshes_the_trust_store() {
        assert_eq!(
            plan_for(r#"{"ca_certs": {}}"#, "ubuntu").unwrap(),
            vec![Step::UpdateCaCerts {
                argv: vec!["update-ca-certificates".to_owned()],
            }]
        );
    }

    #[test]
    fn a_non_mapping_reproduces_upstreams_unformatted_message() {
        assert_eq!(
            plan_for(r#"{"ca_certs": []}"#, "ubuntu").unwrap_err(),
            "unexpected type: {ca_cert_cfg}"
        );
    }

    #[test]
    fn certificates_are_numbered_from_one() {
        let steps =
            plan_for(r#"{"ca_certs": {"trusted": ["a", "b"]}}"#, "ubuntu").unwrap();
        assert_eq!(
            steps[0],
            Step::WriteCert {
                path: "/usr/local/share/ca-certificates/cloud-init-ca-cert-1.crt"
                    .to_owned(),
                contents: "a".to_owned(),
            }
        );
        assert_eq!(
            steps[1],
            Step::WriteCert {
                path: "/usr/local/share/ca-certificates/cloud-init-ca-cert-2.crt"
                    .to_owned(),
                contents: "b".to_owned(),
            }
        );
    }

    #[test]
    fn a_scalar_trusted_value_becomes_one_certificate() {
        let steps = plan_for(r#"{"ca_certs": {"trusted": 5}}"#, "ubuntu").unwrap();
        assert_eq!(
            steps[0],
            Step::WriteCert {
                path: "/usr/local/share/ca-certificates/cloud-init-ca-cert-1.crt"
                    .to_owned(),
                contents: "5".to_owned(),
            }
        );
    }

    #[test]
    fn removing_defaults_on_ubuntu_deselects_and_answers_debconf() {
        let steps =
            plan_for(r#"{"ca_certs": {"remove_defaults": true}}"#, "ubuntu").unwrap();
        assert_eq!(
            steps[0],
            Step::DisableSystemCaCerts {
                path: "/etc/ca-certificates.conf".to_owned(),
            }
        );
        assert_eq!(
            steps[1],
            Step::DebconfSetSelections {
                data: "ca-certificates ca-certificates/trust_new_crts select no"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn removing_defaults_on_rhel_empties_both_directories() {
        let steps =
            plan_for(r#"{"ca_certs": {"remove_defaults": true}}"#, "rhel").unwrap();
        assert_eq!(
            steps[0],
            Step::RemoveDirContents {
                path: "/etc/pki/ca-trust/".to_owned(),
            }
        );
        assert_eq!(
            steps[1],
            Step::RemoveDirContents {
                path: "/usr/share/pki/ca-trust-source/".to_owned(),
            }
        );
    }

    #[test]
    fn centos_shares_rhels_paths_but_not_its_removal_arm() {
        let steps =
            plan_for(r#"{"ca_certs": {"remove_defaults": true}}"#, "centos").unwrap();
        assert_eq!(
            steps,
            vec![Step::UpdateCaCerts {
                argv: vec!["update-ca-trust".to_owned()],
            }]
        );
    }

    #[test]
    fn the_fedora_filename_carries_a_directory_component() {
        assert_eq!(
            distro_config("fedora").full_path(),
            "/usr/share/pki/ca-trust-source/anchors/cloud-init-ca-cert-{cert_index}.crt"
        );
    }

    #[test]
    fn deselect_comments_out_only_enabled_entries() {
        let original =
            "# a comment\n\n!already/off\nmozilla/one.crt\nmozilla/two.crt\n";
        assert_eq!(
            deselect(original),
            format!(
                "# a comment\n\n!already/off\n{HEADER_COMMENT}\n!mozilla/one.crt\n!mozilla/two.crt\n"
            )
        );
    }

    #[test]
    fn deselect_is_idempotent() {
        let once = deselect("mozilla/one.crt\n");
        assert_eq!(deselect(&once), once);
    }

    #[test]
    fn deselect_terminates_a_file_that_had_no_final_newline() {
        assert_eq!(
            deselect("mozilla/one.crt"),
            format!("{HEADER_COMMENT}\n!mozilla/one.crt\n")
        );
    }

    #[test]
    fn an_empty_selection_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca-certificates.conf");
        std::fs::write(&path, b"").unwrap();
        disable_system_ca_certs(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn a_missing_selection_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        disable_system_ca_certs(&dir.path().join("absent.conf")).unwrap();
    }

    #[test]
    fn run_writes_certificates_under_the_root_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let steps =
            plan_for(r#"{"ca_certs": {"trusted": ["PEM"]}}"#, "ubuntu").unwrap();
        run(&steps, dir.path(), &mut Logger::silent()).unwrap();
        let written = dir
            .path()
            .join("usr/local/share/ca-certificates/cloud-init-ca-cert-1.crt");
        assert_eq!(std::fs::read_to_string(&written).unwrap(), "PEM");
    }

    #[test]
    fn delete_dir_contents_keeps_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/deep.crt"), b"x").unwrap();
        std::fs::write(dir.path().join("flat.crt"), b"x").unwrap();
        delete_dir_contents(dir.path()).unwrap();
        assert!(dir.path().is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
