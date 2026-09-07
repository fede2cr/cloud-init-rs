//! Port of `sources/DataSourceConfigDrive.py`.
//!
//! The seed-directory half only: `find_candidate_devs` needs `blkid` and
//! `mount_cb` needs root, so the block-device path is deferred alongside
//! `NoCloud`'s `cidata` (deviation 71).

use std::path::PathBuf;

use ci_config::Value;
use ci_core::Paths;

use crate::helpers::openstack;
use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `DataSourceConfigDrive.DEFAULT_IID`.
const DEFAULT_IID: &str = "iid-dsconfigdrive";

#[derive(Debug)]
pub struct ConfigDrive;

impl ConfigDrive {
    /// The directories upstream tries before it reaches for block devices.
    fn seed_dirs(paths: &Paths) -> [PathBuf; 2] {
        [
            paths.seed_dir().join("config_drive"),
            PathBuf::from("/config-drive"),
        ]
    }
}

impl Probe for ConfigDrive {
    fn dsname(&self) -> &'static str {
        "ConfigDrive"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceConfigDrive"
    }

    fn display(&self) -> String {
        // `__str__` appends its bracket groups to the base class's `"%s "`.
        format!("{} ", self.class_name())
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let mut found = None;
        for dir in Self::seed_dirs(ctx.paths) {
            if !dir.is_dir() {
                continue;
            }
            match openstack::read_config_drive(&dir) {
                Ok(results) => {
                    found = Some((dir, results));
                    break;
                }
                Err(err) => {
                    ctx.logger.warning(
                        "DataSourceConfigDrive.py",
                        &format!(
                            "Failed reading config drive from {}: {err}",
                            dir.display()
                        ),
                    );
                }
            }
        }
        let (source, mut results) = found?;

        results
            .metadata
            .entry("instance-id".to_owned())
            .or_insert_with(|| Value::String(DEFAULT_IID.to_owned()));

        // A v1 drive offers `pass`, which is not a valid dsmode, so upstream
        // warns and falls back to `net` (bug B39). The port keeps that.
        let version_default =
            (results.version == 1).then(|| Value::String("pass".to_owned()));
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let dsmode = DsMode::determine(
            &[
                results.dsmode.as_deref().map(Value::from).as_ref(),
                ds_cfg.get("dsmode"),
                version_default.as_ref(),
            ],
            DsMode::Network,
            ctx.logger,
        );
        if dsmode == DsMode::Disabled {
            return None;
        }

        let instance_id = results
            .metadata
            .get("instance-id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_IID)
            .to_owned();

        let source = source.display().to_string();
        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode,
            instance_id,
            platform_type: "openstack".to_owned(),
            subplatform: subplatform(&source),
            cloud_name_default: METADATA_UNKNOWN.to_owned(),
            detail: detail(&source, dsmode, results.version),
            metadata: std::mem::take(&mut results.metadata),
            userdata_raw: results.userdata,
            vendordata_raw: crate::types::convert_vendordata(
                results.vendordata.as_ref(),
                ctx,
                "DataSourceConfigDrive.py",
                "vendor-data",
            ),
            vendordata2_raw: crate::types::convert_vendordata(
                results.vendordata2.as_ref(),
                ctx,
                "DataSourceConfigDrive.py",
                "vendor-data2",
            ),
            network_config: results.networkdata,
        })
    }
}

/// `_get_subplatform`.
fn subplatform(source: &str) -> String {
    let slug = if source.starts_with("/dev") {
        "config-disk"
    } else {
        "seed-dir"
    };
    format!("{slug} ({source})")
}

/// The `[dsmode,ver=N][source=...]` groups `__str__` appends.
fn detail(source: &str, dsmode: DsMode, version: u8) -> String {
    format!(" [{dsmode},ver={version}][source={source}]")
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

    struct Harness {
        _root: tempfile::TempDir,
        paths: Paths,
        sys_cfg: ci_config::Object,
        logger: ci_log::Logger,
    }

    impl Harness {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let cloud_dir = root.path().join("cloud");
            std::fs::create_dir_all(cloud_dir.join("seed/config_drive")).unwrap();
            let paths = Paths {
                cloud_dir,
                run_dir: root.path().join("run"),
                templates_dir: root.path().join("templates"),
                docs_dir: root.path().join("docs"),
            };
            Self {
                _root: root,
                paths,
                sys_cfg: ci_config::Object::new(),
                logger: ci_log::Logger::silent(),
            }
        }

        fn seed(&self) -> PathBuf {
            self.paths.seed_dir().join("config_drive")
        }

        fn write(&self, name: &str, body: &str) {
            let path = self.seed().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        fn get_data(&mut self) -> Option<Datasource> {
            let mut reporter = ci_report::Reporter::silent();
            let mut ctx = Context {
                sys_cfg: &self.sys_cfg,
                paths: &self.paths,
                cmdline: "",
                limits: ci_config::Limits::default(),
                logger: &mut self.logger,
                reporter: &mut reporter,
            };
            ConfigDrive.get_data(&mut ctx)
        }
    }

    #[test]
    fn a_v2_seed_directory_is_claimed() {
        let mut harness = Harness::new();
        harness.write(
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc", "hostname": "host1"}"#,
        );
        harness.write("openstack/2018-08-27/user_data", "#cloud-config\n");

        let found = harness.get_data().unwrap();

        assert_eq!(found.instance_id, "i-abc");
        assert_eq!(found.dsmode, DsMode::Network);
        assert_eq!(found.platform_type, "openstack");
        assert_eq!(found.userdata_raw.unwrap(), b"#cloud-config\n");
        assert_eq!(
            found.subplatform,
            format!("seed-dir ({})", harness.seed().display())
        );
    }

    #[test]
    fn an_absent_seed_directory_finds_nothing() {
        let mut harness = Harness::new();

        assert!(harness.get_data().is_none());
    }

    #[test]
    fn network_data_json_becomes_the_network_config() {
        let mut harness = Harness::new();
        harness.write(
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc"}"#,
        );
        harness.write(
            "openstack/2018-08-27/network_data.json",
            r#"{"links": [], "networks": []}"#,
        );

        let found = harness.get_data().unwrap();

        assert_eq!(
            found.network_config.unwrap(),
            serde_json::json!({"links": [], "networks": []})
        );
    }

    #[test]
    fn a_dsmode_of_disabled_declines_the_instance() {
        let mut harness = Harness::new();
        harness.write(
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc", "meta": {"dsmode": "disabled"}}"#,
        );

        assert!(harness.get_data().is_none());
    }

    #[test]
    fn a_vendor_data_mapping_is_reduced_to_its_cloud_init_key() {
        let mut harness = Harness::new();
        harness.write(
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc"}"#,
        );
        harness.write(
            "openstack/2018-08-27/vendor_data.json",
            r##"{"cloud-init": "#cloud-config\nruncmd: [ls]\n"}"##,
        );

        let found = harness.get_data().unwrap();

        assert_eq!(
            found.vendordata_raw.unwrap(),
            b"#cloud-config\nruncmd: [ls]\n"
        );
    }

    #[test]
    fn a_v1_drive_reports_version_one_and_falls_back_to_net() {
        let mut harness = Harness::new();
        harness.write("meta.js", r#"{"instance-id": "i-v1"}"#);

        let found = harness.get_data().unwrap();

        assert_eq!(found.instance_id, "i-v1");
        assert_eq!(found.dsmode, DsMode::Network);
        assert!(found.detail.contains("ver=1"), "{}", found.detail);
    }

    #[test]
    fn a_drive_without_an_instance_id_gets_the_default() {
        let mut harness = Harness::new();
        harness.write("meta.js", "{}");

        let found = harness.get_data().unwrap();

        assert_eq!(found.instance_id, DEFAULT_IID);
    }
}
