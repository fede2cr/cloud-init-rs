//! `DataSourceAzure` as a [`Probe`]: `ds_detect` and `_get_data`.
//!
//! This is the piece that makes everything else in [`super`] reachable from a
//! boot. The crawl it drives is [`super::crawl::crawl_metadata`] and the host
//! it drives it against is [`super::host::Host`].

use ci_config::{Object, Value};

use super::{crawl, ds, host::Host};
use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

const SOURCE: &str = "DataSourceAzure.py";

#[derive(Debug)]
pub struct Azure;

impl Probe for Azure {
    fn dsname(&self) -> &'static str {
        ds::DS_NAME
    }

    fn class_name(&self) -> &'static str {
        "DataSourceAzure"
    }

    fn display(&self) -> String {
        // `__str__` is `"%s [seed=%s]"`, and before the crawl the seed is None.
        format!("{} [seed=None]", self.class_name())
    }

    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        let seed_dir = ctx.paths.seed_dir().join("azure");
        ds::ds_detect(Some(&seed_dir), ctx.logger)
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ds::ds_config(ctx.sys_cfg);
        let data_dir = ds_cfg
            .get("data_dir")
            .and_then(Value::as_str)
            .unwrap_or(ds::AGENT_SEED_DIR)
            .to_owned();
        let mut host = Host::new(ctx.paths, &ds_cfg);

        let crawled = match crawl::crawl_metadata(
            &mut host,
            std::path::Path::new(&data_dir),
            false,
            ctx.reporter,
            ctx.logger,
        ) {
            Ok(crawled) => crawled,
            Err(error) => {
                report_failure(&error, &mut host, ctx);
                ctx.logger.error(SOURCE, &error.to_string());
                return None;
            }
        };

        // walinux agent writes files world readable, but expects the directory
        // to be protected.
        ds::write_files(
            std::path::Path::new(&data_dir),
            &crawled.files,
            0o700,
            ctx.logger,
        );

        let metadata = ci_config::merge::merge_many(
            vec![crawled.metadata.clone(), ds::default_metadata()],
            false,
        );
        let imds = crawled
            .metadata
            .get("imds")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut system_uuid = None;
        if let Ok(uuid) = crawl::Platform::system_uuid(&mut host) {
            system_uuid = Some(uuid);
        }
        let instance_id = ds::instance_id(
            &metadata,
            system_uuid.as_deref().unwrap_or(METADATA_UNKNOWN),
        );

        // The interfaces are what `_generate_network_config` matches IMDS'
        // NICs against by MAC, and what its Hyper-V VF filtering needs the
        // driver names for; without them a multi-NIC VM gets one interface.
        let interfaces = host.interfaces();
        let network_config =
            ds::generate_network_config(&ds_cfg, Some(&imds), &interfaces, ctx.logger);

        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode: DsMode::Network,
            instance_id,
            metadata,
            userdata_raw: Some(crawled.userdata),
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config,
            platform_type: "azure".to_owned(),
            subplatform: ds::subplatform(Some(&crawled.seed)),
            cloud_name_default: "azure".to_owned(),
            detail: format!(" [seed={}]", crawled.seed),
        })
    }

    fn check_instance_id(&self, ctx: &mut Context<'_>, current: &str) -> Option<bool> {
        let system_uuid = super::identity::query_system_uuid(ctx.logger);
        Some(ds::check_instance_id(current, system_uuid.as_deref()))
    }
}

/// `_get_data`'s two `except` arms: whatever went wrong, the host is told
/// before the datasource gives up.
///
/// Only the KVP half of upstream's `_report_failure` happens here. The fabric
/// half needs an ephemeral lease this path has already torn down, and the
/// wireserver call it makes is [`super::wire::report_failure`] (deviation 54).
fn report_failure(error: &crawl::Error, host: &mut Host, ctx: &mut Context<'_>) {
    let reportable = match error {
        crawl::Error::Reportable(reportable) => (**reportable).clone(),
        crawl::Error::InvalidMetadata(message) => {
            super::errors::unhandled_exception(message)
        }
        // Pre-provisioning is a control-flow signal upstream, not a failure.
        crawl::Error::Preprovisioning(_) => return,
    };
    let vm_id = crawl::Platform::system_uuid(host).ok().and_then(|uuid| {
        let gen1 = crawl::Platform::is_gen1(host);
        super::identity::convert_system_uuid_to_vm_id(&uuid, gen1, ctx.logger)
    });
    super::kvp::report_failure_to_host(
        ctx.reporter,
        &reportable,
        vm_id.as_deref(),
        ctx.logger,
    );
}

/// `self.cfg` — the config the crawl produced, plus the ephemeral-disk
/// defaults when that disk is actually present.
///
/// Kept apart from [`Probe::get_data`] because `Datasource` has no `cfg` field
/// yet; the stage driver will need this when it does.
#[must_use]
pub fn datasource_config(crawled: &Object, resource_disk_exists: bool) -> Object {
    if resource_disk_exists {
        ci_config::merge::merge_many(
            vec![crawled.clone(), builtin_cloud_ephemeral_disk_config()],
            false,
        )
    } else {
        crawled.clone()
    }
}

/// `BUILTIN_CLOUD_EPHEMERAL_DISK_CONFIG`.
///
/// The device names are the `ephemeral0` aliases from `BUILTIN_DS_CONFIG`, not
/// paths; `device_name_to_device` resolves them.
#[must_use]
pub fn builtin_cloud_ephemeral_disk_config() -> Object {
    let mut ephemeral = Object::new();
    ephemeral.insert("table_type".to_owned(), Value::from("gpt"));
    ephemeral.insert("layout".to_owned(), Value::Array(vec![Value::from(100)]));
    ephemeral.insert("overwrite".to_owned(), Value::Bool(true));

    let mut disk_setup = Object::new();
    disk_setup.insert("ephemeral0".to_owned(), Value::Object(ephemeral));

    let mut fs = Object::new();
    fs.insert("filesystem".to_owned(), Value::from(ds::DEFAULT_FS));
    fs.insert("device".to_owned(), Value::from("ephemeral0.1"));

    let mut config = Object::new();
    config.insert("disk_setup".to_owned(), Value::Object(disk_setup));
    config.insert("fs_setup".to_owned(), Value::Array(vec![Value::Object(fs)]));
    config
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
    fn the_ephemeral_disk_config_is_only_merged_when_the_disk_is_there() {
        let mut crawled = Object::new();
        crawled.insert("system_info".to_owned(), Value::from("kept"));
        assert!(!datasource_config(&crawled, false).contains_key("disk_setup"));

        let merged = datasource_config(&crawled, true);
        assert_eq!(merged["system_info"], "kept");
        assert!(merged["disk_setup"].get("ephemeral0").is_some());
        assert_eq!(merged["fs_setup"][0]["device"], "ephemeral0.1");
    }

    #[test]
    fn the_probe_names_itself_the_way_the_search_log_does() {
        let probe = Azure;
        assert_eq!(probe.dsname(), "Azure");
        assert_eq!(probe.class_name(), "DataSourceAzure");
        assert_eq!(probe.display(), "DataSourceAzure [seed=None]");
    }
}
