//! Port of `sources/DataSourceOpenStack.py`.
//!
//! The network variant only: `DataSourceOpenStackLocal` differs from it by
//! bringing up an ephemeral DHCP lease first, which belongs with the network
//! layer (deviation 73).

use ci_config::{Object, Value};

use crate::helpers::openstack;
use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `DataSourceOpenStack.DEFAULT_IID`.
const DEFAULT_IID: &str = "iid-dsopenstack";

/// `VALID_DMI_PRODUCT_NAMES`.
const VALID_DMI_PRODUCT_NAMES: &[&str] = &["OpenStack Nova", "OpenStack Compute"];

/// `VALID_DMI_ASSET_TAGS`: the product names plus the clouds that rebrand them.
const VALID_DMI_ASSET_TAGS: &[&str] = &[
    "OpenStack Nova",
    "OpenStack Compute",
    "HUAWEICLOUD",
    "OpenTelekomCloud",
    "Samsung Cloud Platform",
    "SAP CCloud VM",
];

/// `DEF_MD_URLS`, less the link-local IPv6 entry, which needs the fallback
/// interface the port has no network layer to ask for (deviation 73).
const DEFAULT_METADATA_URL: &str = "http://169.254.169.254";

#[derive(Debug)]
pub struct OpenStack;

impl Probe for OpenStack {
    fn dsname(&self) -> &'static str {
        "OpenStack"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceOpenStack"
    }

    fn display(&self) -> String {
        format!("{} ", self.class_name())
    }

    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        // Non-Intel CPUs do not report DMI product names reliably, so the
        // platform is assumed present rather than ruled out.
        if !ci_core::sysinfo::is_x86() {
            return true;
        }
        let product = crate::dmi::read_dmi_data("system-product-name", ctx.logger);
        if product
            .as_deref()
            .is_some_and(|name| VALID_DMI_PRODUCT_NAMES.contains(&name))
        {
            return true;
        }
        let asset_tag = crate::dmi::read_dmi_data("chassis-asset-tag", ctx.logger);
        if asset_tag
            .as_deref()
            .is_some_and(|tag| VALID_DMI_ASSET_TAGS.contains(&tag))
        {
            return true;
        }
        // The Oracle branch is absent because the Oracle datasource is.
        ci_core::container::proc_env(std::path::Path::new("/proc/1/environ"))
            .iter()
            .any(|(name, value)| name == "product_name" && value == "OpenStack Nova")
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let config = crate::types::url_config(&ds_cfg);

        let address = Self::wait_for_metadata_service(&ds_cfg, &config, ctx)?;
        let results = match openstack::read_metadata_service(&address, &config) {
            Ok(results) => results,
            Err(err) => {
                let message = match err {
                    openstack::Error::NonReadable(message) => message,
                    openstack::Error::Broken(_) => {
                        format!("Broken metadata address {address}")
                    }
                };
                ctx.logger.warning("DataSourceOpenStack.py", &message);
                return None;
            }
        };

        let dsmode = DsMode::determine(
            &[results.dsmode.as_deref().map(Value::from).as_ref()],
            DsMode::Network,
            ctx.logger,
        );
        if dsmode == DsMode::Disabled {
            return None;
        }

        let mut metadata = results.metadata;
        metadata
            .entry("instance-id".to_owned())
            .or_insert_with(|| Value::String(DEFAULT_IID.to_owned()));
        let instance_id = metadata
            .get("instance-id")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_IID)
            .to_owned();

        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode,
            instance_id,
            platform_type: "openstack".to_owned(),
            subplatform: format!("metadata ({address})"),
            cloud_name_default: METADATA_UNKNOWN.to_owned(),
            detail: format!(" [{dsmode},ver={}]", results.version),
            metadata,
            userdata_raw: results.userdata,
            vendordata_raw: crate::types::convert_vendordata(
                results.vendordata.as_ref(),
                ctx,
                "DataSourceOpenStack.py",
                "vendor-data",
            ),
            vendordata2_raw: crate::types::convert_vendordata(
                results.vendordata2.as_ref(),
                ctx,
                "DataSourceOpenStack.py",
                "vendor-data2",
            ),
            network_config: results.networkdata,
        })
    }
}

impl OpenStack {
    /// `wait_for_metadata_service`: the first base URL whose `openstack`
    /// index answers.
    ///
    /// Upstream probes the candidates concurrently and against a wall-clock
    /// `max_wait`; the port tries them in order, which is the same answer for
    /// the single-URL default (deviation 73).
    fn wait_for_metadata_service(
        ds_cfg: &Object,
        config: &ci_url::Config,
        ctx: &mut Context<'_>,
    ) -> Option<String> {
        let urls = Self::metadata_urls(ds_cfg);

        for url in &urls {
            let probe = ci_url::url::combine_url(url, &["openstack"]);
            if ci_url::readurl(&probe, config).is_ok_and(|response| response.ok()) {
                ctx.logger.debug(
                    "DataSourceOpenStack.py",
                    &format!("Using metadata source: '{url}'"),
                );
                return Some(url.clone());
            }
        }
        ctx.logger.debug(
            "DataSourceOpenStack.py",
            &format!("Giving up on OpenStack md from {urls:?}"),
        );
        None
    }

    /// `BUILTIN_DS_CONFIG["metadata_urls"]`, overridable per datasource.
    fn metadata_urls(ds_cfg: &Object) -> Vec<String> {
        ds_cfg
            .get("metadata_urls")
            .and_then(Value::as_array)
            .map_or_else(
                || vec![DEFAULT_METADATA_URL.to_owned()],
                |list| {
                    list.iter()
                        .filter_map(|url| url.as_str().map(ToOwned::to_owned))
                        .collect()
                },
            )
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
    fn the_default_metadata_url_is_used_when_the_config_names_none() {
        let ds_cfg = Object::new();

        assert_eq!(
            OpenStack::metadata_urls(&ds_cfg),
            vec![DEFAULT_METADATA_URL.to_owned()]
        );
    }

    #[test]
    fn the_datasource_config_can_name_its_own_metadata_urls() {
        let mut ds_cfg = Object::new();
        ds_cfg.insert(
            "metadata_urls".to_owned(),
            serde_json::json!(["http://a", "http://b"]),
        );

        assert_eq!(
            OpenStack::metadata_urls(&ds_cfg),
            vec!["http://a".to_owned(), "http://b".to_owned()]
        );
    }
}
