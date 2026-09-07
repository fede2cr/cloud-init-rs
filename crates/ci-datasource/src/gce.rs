//! Port of `sources/DataSourceGCE.py`.
//!
//! The network variant only: `DataSourceGCELocal` walks candidate NICs taking
//! an ephemeral DHCP lease on each, which belongs with the network layer
//! (deviation 74).

use ci_config::{Object, Value};

use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `MD_V1_URL`.
const MD_V1_URL: &str = "http://metadata.google.internal/computeMetadata/v1/";

/// `HEADERS`. The metadata service rejects a request without it.
const FLAVOR_HEADER: (&str, &str) = ("Metadata-Flavor", "Google");

/// `url_map`, as `(key, path, required, recursive)`.
const URL_MAP: &[(&str, &str, bool, bool)] = &[
    ("instance-id", "instance/id", true, false),
    ("availability-zone", "instance/zone", true, false),
    ("local-hostname", "instance/hostname", true, false),
    ("instance-data", "instance/attributes", false, true),
    ("project-data", "project/attributes", false, true),
];

/// What `read_md` returns: upstream's `ret` dict, minus the
/// `platform_reports_gce` flag, which only picks the log level.
#[derive(Debug, Default)]
pub struct ReadMd {
    pub metadata: Option<Object>,
    pub userdata: Option<Vec<u8>>,
    pub reason: Option<String>,
}

impl ReadMd {
    fn refused(reason: String) -> Self {
        Self {
            reason: Some(reason),
            ..Self::default()
        }
    }
}

/// `read_md`, without the `platform_check` (the caller has already made it)
/// and without `is_resolvable_url` (deviation 74).
#[must_use]
pub fn read_md(address: &str, config: &ci_url::Config) -> ReadMd {
    let mut md = Object::new();
    for (key, path, required, recursive) in URL_MAP {
        let mut url = format!("{address}{path}");
        if *recursive {
            url.push_str("/?recursive=True");
        }
        let value = ci_url::readurl(&url, config)
            .ok()
            .filter(|response| response.code == 200)
            .map(|response| String::from_utf8_lossy(&response.contents).into_owned());
        match value {
            Some(value) => {
                md.insert((*key).to_owned(), Value::String(value));
            }
            None if *required => {
                return ReadMd::refused(format!(
                    "required key {key} returned nothing. not GCE"
                ));
            }
            None => {
                md.insert((*key).to_owned(), Value::Null);
            }
        }
    }

    let instance_data = parse_attributes(md.get("instance-data"));
    let project_data = parse_attributes(md.get("project-data"));

    let mut userdata = None;
    if let Some(text) = instance_data.get("user-data").and_then(Value::as_str) {
        userdata = Some(
            match instance_data
                .get("user-data-encoding")
                .and_then(Value::as_str)
            {
                Some("base64") => ci_core::b64::decode(text).unwrap_or_default(),
                _ => text.as_bytes().to_vec(),
            },
        );
    }

    md.insert(
        "public-keys-data".to_owned(),
        Value::Array(public_keys_data(&instance_data, &project_data)),
    );

    // `instance/zone` is `projects/<n>/zones/<zone>`.
    if let Some(zone) = md.get("availability-zone").and_then(Value::as_str) {
        let short = zone.rsplit('/').next().unwrap_or(zone).to_owned();
        md.insert("availability-zone".to_owned(), Value::String(short));
    }

    md.insert("instance-data".to_owned(), Value::Object(instance_data));
    md.insert("project-data".to_owned(), Value::Object(project_data));

    ReadMd {
        metadata: Some(md),
        userdata,
        reason: None,
    }
}

/// The `valid_keys` list, flattened to lines.
fn public_keys_data(instance_data: &Object, project_data: &Object) -> Vec<Value> {
    let mut sources = vec![instance_data.get("sshKeys"), instance_data.get("ssh-keys")];
    let blocked = instance_data
        .get("block-project-ssh-keys")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if !blocked && instance_data.get("sshKeys").is_none() {
        sources.push(project_data.get("ssh-keys"));
        sources.push(project_data.get("sshKeys"));
    }
    sources
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .lines()
        .map(|line| Value::String(line.to_owned()))
        .collect()
}

/// `json.loads(md[...] or "{}")`, which upstream lets raise on bad JSON.
fn parse_attributes(value: Option<&Value>) -> Object {
    value
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default()
}

/// `platform_reports_gce`.
fn platform_reports_gce(ctx: &mut Context<'_>) -> bool {
    let product = crate::dmi::read_dmi_data("system-product-name", ctx.logger);
    if matches!(product.as_deref(), Some("Google Compute Engine" | "Google")) {
        return true;
    }
    let serial = crate::dmi::read_dmi_data("system-serial-number", ctx.logger);
    if serial
        .as_deref()
        .is_some_and(|serial| serial.starts_with("GoogleCloud-"))
    {
        return true;
    }
    ctx.logger.debug(
        "DataSourceGCE.py",
        &format!(
            "Not running on google cloud. product-name={} serial={}",
            product.as_deref().unwrap_or("N/A"),
            serial.as_deref().unwrap_or("N/A")
        ),
    );
    false
}

#[derive(Debug)]
pub struct Gce;

impl Probe for Gce {
    fn dsname(&self) -> &'static str {
        "GCE"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceGCE"
    }

    fn display(&self) -> String {
        format!("{} ", self.class_name())
    }

    fn default_update_events(&self) -> crate::event::Events {
        crate::event::events(
            crate::event::Scope::Network,
            &[
                crate::event::Type::BootNewInstance,
                crate::event::Type::Boot,
            ],
        )
    }

    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        platform_reports_gce(ctx)
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let address = ds_cfg
            .get("metadata_url")
            .and_then(Value::as_str)
            .unwrap_or(MD_V1_URL)
            .to_owned();

        let mut config = crate::types::url_config(&ds_cfg);
        config.headers = vec![(FLAVOR_HEADER.0.to_owned(), FLAVOR_HEADER.1.to_owned())];

        let result = read_md(&address, &config);
        let Some(metadata) = result.metadata else {
            let reason = result.reason.unwrap_or_default();
            // Upstream warns when DMI said GCE and debugs otherwise.
            if platform_reports_gce(ctx) {
                ctx.logger.warning("DataSourceGCE.py", &reason);
            } else {
                ctx.logger.debug("DataSourceGCE.py", &reason);
            }
            return None;
        };

        let instance_id = metadata
            .get("instance-id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode: DsMode::Network,
            instance_id,
            platform_type: "gce".to_owned(),
            subplatform: format!("metadata ({address})"),
            cloud_name_default: METADATA_UNKNOWN.to_owned(),
            detail: String::new(),
            metadata,
            userdata_raw: result.userdata,
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config: None,
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

    fn attrs(json: &str) -> Object {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn instance_keys_and_project_keys_are_both_offered_by_default() {
        let instance = attrs(r#"{"ssh-keys": "u:key1"}"#);
        let project = attrs(r#"{"ssh-keys": "u:key2"}"#);

        let keys = public_keys_data(&instance, &project);

        assert_eq!(keys, vec![Value::from("u:key1"), Value::from("u:key2")]);
    }

    #[test]
    fn the_legacy_instance_ssh_keys_attribute_shuts_out_the_project() {
        let instance = attrs(r#"{"sshKeys": "u:key1"}"#);
        let project = attrs(r#"{"ssh-keys": "u:key2"}"#);

        let keys = public_keys_data(&instance, &project);

        assert_eq!(keys, vec![Value::from("u:key1")]);
    }

    #[test]
    fn blocking_project_keys_is_case_insensitive() {
        let project = attrs(r#"{"ssh-keys": "u:key2"}"#);

        for blocked in ["true", "TRUE", "True"] {
            let instance =
                attrs(&format!(r#"{{"block-project-ssh-keys": "{blocked}"}}"#));
            assert!(public_keys_data(&instance, &project).is_empty());
        }
    }

    #[test]
    fn no_keys_anywhere_yields_an_empty_list() {
        let instance = attrs("{}");

        assert!(public_keys_data(&instance, &instance).is_empty());
    }

    #[test]
    fn attributes_that_are_absent_or_unparseable_read_as_empty() {
        assert!(parse_attributes(None).is_empty());
        assert!(parse_attributes(Some(&Value::Null)).is_empty());
        assert!(parse_attributes(Some(&Value::from("{not json"))).is_empty());
        assert_eq!(
            parse_attributes(Some(&Value::from(r#"{"a": "b"}"#))).len(),
            1
        );
    }
}
