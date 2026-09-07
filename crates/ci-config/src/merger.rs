//! Port of `helpers.ConfigMerger` and `stages.Init._read_cfg`.
//!
//! Precedence, highest first: `--file` configs, the `CLOUD_CFG` environment
//! config, the cloud-config cached under the instance directory, then
//! [`crate::read::fetch_base_config`]. The datasource layer that sits between
//! the last two upstream has nothing to contribute until Phase 3.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::builtin;
use crate::merge::merge_many;
use crate::read::{fetch_base_config, read_conf};
use crate::yaml::Limits;
use crate::Object;

/// The directories the merge itself needs, before `ci_core::Paths` exists.
#[derive(Debug, Clone)]
struct BootPaths {
    cloud_dir: PathBuf,
    run_dir: PathBuf,
}

impl Default for BootPaths {
    fn default() -> Self {
        Self {
            cloud_dir: PathBuf::from("/var/lib/cloud"),
            run_dir: PathBuf::from(builtin::DEFAULT_RUN_DIR),
        }
    }
}

impl BootPaths {
    fn from_cfg(cfg: &Object) -> Self {
        let mut paths = Self::default();
        let Some(configured) = paths_cfg(cfg) else {
            return paths;
        };
        let get = |key: &str| {
            configured
                .get(key)
                .and_then(Value::as_str)
                .map(PathBuf::from)
        };
        if let Some(dir) = get("cloud_dir") {
            paths.cloud_dir = dir;
        }
        if let Some(dir) = get("run_dir") {
            paths.run_dir = dir;
        }
        paths
    }
}

fn paths_cfg(cfg: &Object) -> Option<&Object> {
    cfg.get("system_info")?.get("paths")?.as_object()
}

/// `Init._read_cfg`: the whole merged system config.
///
/// The config decides where `run_dir` is, but `run_dir` is where part of the
/// config lives, so upstream reads once against the default layout and then
/// reads again if that first pass moved `run_dir`.
#[must_use]
pub fn read_cfg(extra_files: &[PathBuf], limits: Limits) -> Object {
    let initial = bootstrap(&BootPaths::default(), extra_files, limits);
    let run_dir = paths_cfg(&initial)
        .and_then(|p| p.get("run_dir"))
        .and_then(Value::as_str);
    // The re-read is gated on `run_dir` alone, so a config that moves only
    // `cloud_dir` keeps reading its instance config from the default tree
    // (COMPAT.md B30). Reproduced rather than fixed.
    match run_dir {
        None => initial,
        Some(dir) if dir == builtin::DEFAULT_RUN_DIR => initial,
        Some(_) => bootstrap(&BootPaths::from_cfg(&initial), extra_files, limits),
    }
}

/// `ConfigMerger(base_cfg=cfg, include_vendor=False)`.
///
/// `Init._consume_vendordata` re-merges before it reads `vendor_data:`, so the
/// cloud-config the user data just wrote can still turn vendor data off. The
/// vendor cloud-config layers are left out of that decision on purpose: vendor
/// data must not be what enables vendor data.
#[must_use]
pub fn merge_over(base: Object, cloud_dir: &Path, limits: Limits) -> Object {
    let mut sources = Vec::new();
    sources.extend(env_config(limits));
    sources.extend(optional(
        &cloud_dir.join("instance/cloud-config.txt"),
        limits,
    ));
    // `_get_datasource_configs` would sit here. Neither ported datasource
    // overrides `get_config_obj`, so it contributes nothing.
    sources.push(base);
    merge_many(sources, false)
}

/// `Init._read_bootstrap_cfg`, for one already-decided layout.
fn bootstrap(paths: &BootPaths, extra_files: &[PathBuf], limits: Limits) -> Object {
    let sensitive = paths.run_dir.join("instance-data-sensitive.json");
    let base =
        fetch_base_config(&paths.run_dir, Some(&sensitive), limits).unwrap_or_default();

    let mut sources: Vec<Object> = Vec::new();
    // Only the base config is rendered as a template; the layers above it are
    // read with upstream's plain `util.read_conf`.
    sources.extend(extra_files.iter().filter_map(|f| optional(f, limits)));
    sources.extend(env_config(limits));
    sources.extend(instance_configs(&paths.cloud_dir, limits));
    sources.push(base);
    merge_many(sources, false)
}

/// `ConfigMerger._get_env_configs`.
fn env_config(limits: Limits) -> Option<Object> {
    let path = std::env::var_os(builtin::CFG_ENV_NAME)?;
    optional(Path::new(&path), limits)
}

/// `ConfigMerger._get_instance_configs`.
///
/// The order matters: `vendor2` is the dynamic vendor data `OpenStack`
/// supplies, and it has to outrank the static vendor data below it.
fn instance_configs(cloud_dir: &Path, limits: Limits) -> Vec<Object> {
    // The `cloud_config`, `vendor2_cloud_config` and `vendor_cloud_config`
    // entries of `ci_core::paths::Lookup`, which this crate sits below.
    const NAMES: [&str; 3] = [
        "cloud-config.txt",
        "vendor2-cloud-config.txt",
        "vendor-cloud-config.txt",
    ];
    let instance = cloud_dir.join("instance");
    NAMES
        .iter()
        .filter(|name| instance.join(name).is_file())
        .filter_map(|name| optional(&instance.join(name), limits))
        .collect()
}

/// Upstream logs the failure and carries on with the layers it could read.
fn optional(path: &Path, limits: Limits) -> Option<Object> {
    read_conf(path, None, limits).ok()
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

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_env_config_layers_over_the_system_config_instead_of_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("env.cfg");
        write(&env, "locale: en_GB\n");
        // A stand-in for /etc/cloud/cloud.cfg. Reading the real one made this
        // test pass only on a machine that has cloud-init installed, so it
        // failed inside the release build container.
        let system = dir.path().join("cloud.cfg");
        write(&system, "locale: en_US\npreserve_hostname: false\n");

        let base = read_conf(&system, None, Limits::default()).unwrap();
        let merged = merge_many(
            vec![optional(&env, Limits::default()).unwrap(), base],
            false,
        );

        assert_eq!(merged["locale"], "en_GB");
        assert_eq!(merged["preserve_hostname"], false);
    }

    #[test]
    fn the_instance_cloud_config_is_read_in_vendor_precedence_order() {
        let dir = tempfile::tempdir().unwrap();
        let instance = dir.path().join("instance");
        write(&instance.join("cloud-config.txt"), "a: user\nb: user\n");
        write(&instance.join("vendor2-cloud-config.txt"), "b: v2\nc: v2\n");
        write(&instance.join("vendor-cloud-config.txt"), "c: v1\nd: v1\n");

        let merged = merge_many(instance_configs(dir.path(), Limits::default()), false);

        assert_eq!(merged["a"], "user");
        assert_eq!(merged["b"], "user");
        assert_eq!(merged["c"], "v2");
        assert_eq!(merged["d"], "v1");
    }

    #[test]
    fn an_absent_instance_directory_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(instance_configs(dir.path(), Limits::default()).is_empty());
    }

    #[test]
    fn a_layer_that_will_not_parse_is_skipped_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let instance = dir.path().join("instance");
        write(&instance.join("cloud-config.txt"), "a: [unterminated\n");
        write(&instance.join("vendor-cloud-config.txt"), "d: v1\n");

        let merged = merge_many(instance_configs(dir.path(), Limits::default()), false);

        assert_eq!(merged["d"], "v1");
        assert!(!merged.contains_key("a"));
    }

    #[test]
    fn moving_only_cloud_dir_does_not_trigger_the_second_pass() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = merge_many(
            vec![serde_json::from_str::<Object>(&format!(
                r#"{{"system_info": {{"paths": {{"cloud_dir": "{}"}}}}}}"#,
                dir.path().display()
            ))
            .unwrap()],
            false,
        );

        let run_dir = paths_cfg(&cfg).and_then(|p| p.get("run_dir"));

        assert!(run_dir.is_none());
        assert_eq!(BootPaths::from_cfg(&cfg).cloud_dir, dir.path());
    }
}
