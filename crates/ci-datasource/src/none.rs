//! Port of `sources.DataSourceNone`.
//!
//! The last resort: it always claims the instance, with whatever `metadata` and
//! `userdata_raw` the `datasource: None:` config block supplies, and reports a
//! fixed instance id so that a machine with no cloud still gets a stable
//! instance directory.

use ci_config::Value;

use crate::types::{Context, Datasource, DsMode, Probe};

/// `DataSourceNone`.
#[derive(Debug)]
pub struct NoneSource;

impl Probe for NoneSource {
    fn dsname(&self) -> &'static str {
        "None"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceNone"
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let metadata = ds_cfg
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let userdata_raw = ds_cfg
            .get("userdata_raw")
            .and_then(Value::as_str)
            .map(|text| text.as_bytes().to_vec());
        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode: DsMode::Network,
            // `get_instance_id` is overridden upstream, so no metadata key and
            // no config can move it.
            instance_id: "iid-datasource-none".to_owned(),
            metadata,
            userdata_raw,
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config: None,
            platform_type: "none".to_owned(),
            subplatform: "config".to_owned(),
            cloud_name_default: "None".to_owned(),
            detail: String::new(),
        })
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
    use ci_config::Object;
    use ci_core::Paths;
    use ci_log::Logger;

    fn context<'a>(
        sys_cfg: &'a Object,
        paths: &'a Paths,
        logger: &'a mut Logger,
        reporter: &'a mut ci_report::Reporter,
    ) -> Context<'a> {
        Context {
            sys_cfg,
            paths,
            cmdline: "",
            limits: ci_config::Limits::default(),
            logger,
            reporter,
        }
    }

    #[test]
    fn it_always_claims_the_instance_even_with_nothing_configured() {
        let sys_cfg = Object::new();
        let paths = Paths::default();
        let mut logger = Logger::silent();
        let mut reporter = ci_report::Reporter::silent();
        let found = NoneSource
            .get_data(&mut context(&sys_cfg, &paths, &mut logger, &mut reporter))
            .unwrap();
        assert_eq!(found.instance_id, "iid-datasource-none");
        assert_eq!(found.subplatform, "config");
        assert_eq!(found.cloud_name(), "none");
        assert_eq!(found.record(), "DataSourceNone: DataSourceNone");
    }

    #[test]
    fn fallback_metadata_and_userdata_come_from_the_datasource_config() {
        let sys_cfg: Object = serde_json::from_str(
            r##"{"datasource": {"None": {"metadata": {"instance-id": "i-abc"},
                                        "userdata_raw": "#cloud-config\n"}}}"##,
        )
        .unwrap();
        let paths = Paths::default();
        let mut logger = Logger::silent();
        let mut reporter = ci_report::Reporter::silent();
        let found = NoneSource
            .get_data(&mut context(&sys_cfg, &paths, &mut logger, &mut reporter))
            .unwrap();
        assert_eq!(found.metadata["instance-id"], Value::from("i-abc"));
        assert_eq!(
            found.userdata_raw.as_deref(),
            Some(b"#cloud-config\n".as_ref())
        );
        assert_eq!(found.instance_id, "iid-datasource-none");
    }
}
