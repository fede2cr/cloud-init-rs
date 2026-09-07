//! Every path the script can be pointed at, and the environment overrides that
//! point it there.
//!
//! Upstream's paths are shell parameter expansions evaluated at source time
//! (`PATH_SYS_CLASS_DMI_ID=${PATH_SYS_CLASS_DMI_ID:-${PATH_ROOT}/sys/class/dmi/id}`),
//! so an unset variable and an empty one are the same thing. `env_or` keeps
//! that: `:-` substitutes when the value is unset *or* empty.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `PATH_*` variables, resolved once.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub sys_class_dmi_id: PathBuf,
    pub sys_hypervisor: PathBuf,
    pub sys_class_block: PathBuf,
    pub dev_disk: PathBuf,
    pub var_lib_cloud: PathBuf,
    pub di_config: PathBuf,
    pub di_env: PathBuf,
    pub proc_cmdline: PathBuf,
    pub proc_1_cmdline: PathBuf,
    pub proc_1_environ: PathBuf,
    pub proc_uptime: PathBuf,
    pub etc_cloud: PathBuf,
    pub etc_ci_cfg: PathBuf,
    pub etc_ci_cfg_d: PathBuf,
    /// Empty until [`Paths::set_run_path`] has seen the kernel name.
    pub run: PathBuf,
    pub run_ci: PathBuf,
    pub run_ci_cfg: PathBuf,
    pub run_di_result: PathBuf,
}

/// `${NAME:-default}`: the default wins for both unset and empty.
fn env_or(name: &str, default: impl Into<OsString>) -> PathBuf {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(default.into()),
    }
}

/// `${PATH_ROOT}/sub`, built by string concatenation the way the shell does it.
///
/// `Path::join` cannot be used: `PATH_ROOT` is normally empty, and joining an
/// empty path with `/sys/...` would be right only by accident, while joining a
/// non-empty root with an absolute path would discard the root entirely.
fn under(root: &Path, sub: &str) -> PathBuf {
    let mut s = root.as_os_str().to_os_string();
    s.push(sub);
    PathBuf::from(s)
}

impl Paths {
    /// Reads the environment exactly as the script's top-level assignments do.
    #[must_use]
    pub fn from_env() -> Self {
        let root = env_or("PATH_ROOT", "");
        let etc_cloud = env_or("PATH_ETC_CLOUD", under(&root, "/etc/cloud"));
        let etc_ci_cfg = env_or("PATH_ETC_CI_CFG", under(&etc_cloud, "/cloud.cfg"));
        let etc_ci_cfg_d = env_or("PATH_ETC_CI_CFG_D", under(&etc_ci_cfg, ".d"));
        Self {
            sys_class_dmi_id: env_or(
                "PATH_SYS_CLASS_DMI_ID",
                under(&root, "/sys/class/dmi/id"),
            ),
            sys_hypervisor: env_or(
                "PATH_SYS_HYPERVISOR",
                under(&root, "/sys/hypervisor"),
            ),
            sys_class_block: env_or(
                "PATH_SYS_CLASS_BLOCK",
                under(&root, "/sys/class/block"),
            ),
            dev_disk: env_or("PATH_DEV_DISK", under(&root, "/dev/disk")),
            var_lib_cloud: env_or("PATH_VAR_LIB_CLOUD", under(&root, "/var/lib/cloud")),
            di_config: env_or(
                "PATH_DI_CONFIG",
                under(&root, "/etc/cloud/ds-identify.cfg"),
            ),
            di_env: env_or("PATH_DI_ENV", under(&root, "/usr/libexec/ds-identify-env")),
            proc_cmdline: env_or("PATH_PROC_CMDLINE", under(&root, "/proc/cmdline")),
            proc_1_cmdline: env_or(
                "PATH_PROC_1_CMDLINE",
                under(&root, "/proc/1/cmdline"),
            ),
            proc_1_environ: env_or(
                "PATH_PROC_1_ENVIRON",
                under(&root, "/proc/1/environ"),
            ),
            proc_uptime: env_or("PATH_PROC_UPTIME", under(&root, "/proc/uptime")),
            etc_cloud,
            etc_ci_cfg,
            etc_ci_cfg_d,
            run: env_or("PATH_RUN", ""),
            run_ci: env_or("PATH_RUN_CI", ""),
            run_ci_cfg: env_or("PATH_RUN_CI_CFG", ""),
            run_di_result: env_or("PATH_RUN_DI_RESULT", ""),
            root,
        }
    }

    /// `set_run_path`: the run directory depends on the kernel, so it is
    /// resolved only after `uname` has been read.
    pub fn set_run_path(&mut self, kernel_name: &str) {
        if self.run.as_os_str().is_empty() {
            let sub = if kernel_name == "Linux" {
                "/run"
            } else {
                "/var/run"
            };
            self.run = under(&self.root, sub);
        }
        if self.run_ci.as_os_str().is_empty() {
            self.run_ci = under(&self.run, "/cloud-init");
        }
        if self.run_ci_cfg.as_os_str().is_empty() {
            self.run_ci_cfg = under(&self.run_ci, "/cloud.cfg");
        }
        if self.run_di_result.as_os_str().is_empty() {
            self.run_di_result = under(&self.run_ci, "/.ds-identify.result");
        }
    }

    /// `${PATH_ROOT}<sub>`, for the handful of checks that build a path inline.
    #[must_use]
    pub fn in_root(&self, sub: &str) -> PathBuf {
        under(&self.root, sub)
    }

    /// `${PATH_VAR_LIB_CLOUD}<sub>`.
    #[must_use]
    pub fn in_var_lib_cloud(&self, sub: &str) -> PathBuf {
        under(&self.var_lib_cloud, sub)
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

    #[test]
    fn concatenates_rather_than_joining() {
        let root = PathBuf::from("/fake");
        assert_eq!(under(&root, "/etc/cloud"), PathBuf::from("/fake/etc/cloud"));
        assert_eq!(
            under(&PathBuf::from(""), "/etc/cloud"),
            PathBuf::from("/etc/cloud")
        );
    }

    #[test]
    fn run_paths_follow_the_kernel_name() {
        let mut paths = Paths {
            root: PathBuf::from("/fake"),
            run: PathBuf::new(),
            run_ci: PathBuf::new(),
            run_ci_cfg: PathBuf::new(),
            run_di_result: PathBuf::new(),
            ..Paths::from_env()
        };
        paths.set_run_path("FreeBSD");
        assert_eq!(paths.run, PathBuf::from("/fake/var/run"));
        assert_eq!(
            paths.run_di_result,
            PathBuf::from("/fake/var/run/cloud-init/.ds-identify.result")
        );
    }
}
