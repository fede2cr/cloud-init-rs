//! Port of the `sources.DataSource` base class.
//!
//! The base class is half plumbing — `sys_cfg`, `paths`, `ds_cfg` — and half
//! properties derived from whatever `_get_data` happened to store. The plumbing
//! is [`Context`], the stored data and its derived properties are
//! [`Datasource`], and the per-cloud `ds_detect`/`_get_data` pair is [`Probe`].

use std::fmt;

use ci_config::{Limits, Object, Value};
use ci_core::Paths;
use ci_log::Logger;
use ci_report::Reporter;

/// `sources.METADATA_UNKNOWN`.
pub const METADATA_UNKNOWN: &str = "unknown";

/// `sources.METADATA_CLOUD_NAME_KEY`.
pub const METADATA_CLOUD_NAME_KEY: &str = "cloud-name";

/// `sources.normalize_pubkey_data`.
///
/// Every shape a metadata service has ever returned for this key: a newline
/// separated blob, a list, or a mapping of key name to one key or several.
/// Anything else is no keys at all rather than an error, which is the right
/// way round — a machine with no key is recoverable, a boot that stopped here
/// is not.
#[must_use]
pub fn normalize_pubkey_data(pubkey_data: Option<&Value>) -> Vec<String> {
    let Some(data) = pubkey_data.filter(|value| ci_config::option::py_truthy(value))
    else {
        return Vec::new();
    };
    match data {
        Value::String(text) => ci_core::pystr::split_lines(text)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
        Value::Array(items) => items.iter().map(py_str).collect(),
        Value::Object(map) => {
            let mut keys = Vec::new();
            for value in map.values() {
                // A metadata service that answers with a bare string instead
                // of a list is lp:506332, still handled.
                let listed = match value {
                    Value::String(text) => vec![Value::String(text.clone())],
                    Value::Array(items) => items.clone(),
                    _ => continue,
                };
                // The trailing empty entry some services append is dropped.
                keys.extend(
                    listed
                        .iter()
                        .filter(|key| ci_config::option::py_truthy(key))
                        .map(py_str),
                );
            }
            keys
        }
        _ => Vec::new(),
    }
}

/// Python's `str()` of a metadata scalar.
fn py_str(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| ci_config::repr::repr(value), ToOwned::to_owned)
}

/// A dependency a datasource declares against the boot stage it can run in.
///
/// Upstream matches on set equality, not subset: a datasource listed as
/// `(DEP_FILESYSTEM,)` is invisible to the network stage, which searches for
/// `{DEP_FILESYSTEM, DEP_NETWORK}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dep {
    Filesystem,
    Network,
}

/// `sources.DSMODE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsMode {
    Disabled,
    Local,
    Network,
    /// Defined upstream but absent from `VALID_DSMODES`, so it never survives
    /// [`DsMode::determine`].
    Pass,
}

impl DsMode {
    /// The on-the-wire spelling. `Network` is `"net"`, not `"network"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Local => "local",
            Self::Network => "net",
            Self::Pass => "pass",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "disabled" => Some(Self::Disabled),
            "local" => Some(Self::Local),
            "net" => Some(Self::Network),
            "pass" => Some(Self::Pass),
            _ => None,
        }
    }

    /// `DataSource._determine_dsmode`: the first candidate that is set wins,
    /// and an unrecognised one falls back to the default with a warning.
    #[must_use]
    pub fn determine(
        candidates: &[Option<&Value>],
        default: DsMode,
        logger: &mut Logger,
    ) -> DsMode {
        for candidate in candidates.iter().flatten() {
            if candidate.is_null() {
                continue;
            }
            let text = candidate.as_str().map_or_else(
                // Upstream compares whatever it was given against a list of
                // strings, so a non-string is simply never valid.
                || candidate.to_string(),
                ToOwned::to_owned,
            );
            match Self::parse(&text) {
                Some(mode) if mode != Self::Pass => return mode,
                _ => {
                    logger.warning(
                        "__init__.py",
                        &format!(
                            "invalid dsmode '{text}', using default={}",
                            default.as_str()
                        ),
                    );
                    return default;
                }
            }
        }
        default
    }
}

impl fmt::Display for DsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What upstream passes to every datasource constructor, plus the kernel
/// command line, which upstream re-reads from `/proc` at each use.
#[derive(Debug)]
pub struct Context<'a> {
    /// `sys_cfg` — the merged system config, before user-data is folded in.
    pub sys_cfg: &'a Object,
    pub paths: &'a Paths,
    pub cmdline: &'a str,
    pub limits: Limits,
    pub logger: &'a mut Logger,
    /// The registered reporting handlers. Upstream reaches these through a
    /// module-level registry; the port holds them in a value, so a datasource
    /// that has to write telemetry — Azure, through the Hyper-V KVP pool —
    /// needs them handed to it.
    pub reporter: &'a mut Reporter,
}

impl Context<'_> {
    /// `util.get_cfg_by_path(sys_cfg, ("datasource", dsname), {})`.
    #[must_use]
    pub fn ds_cfg(&self, dsname: &str) -> Object {
        self.sys_cfg
            .get("datasource")
            .and_then(|ds| ds.get(dsname))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }

    /// `sys_cfg["datasource_list"]`, as the strings upstream compares.
    #[must_use]
    pub fn datasource_list(&self) -> Vec<String> {
        self.sys_cfg
            .get("datasource_list")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| entry.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A datasource that claimed the instance, and everything read off it.
///
/// Upstream keeps this on the datasource object itself and recomputes the
/// derived properties on every access; they are resolved once here, at the
/// point `_get_data` returns, which is the only point their inputs change.
#[derive(Debug, Clone)]
pub struct Datasource {
    /// `type_utils.obj_name(ds)` — the Python class name, which is what gets
    /// logged and written to the instance's `datasource` file.
    pub class_name: &'static str,
    /// `dsname`, the key `datasource_list` and `datasource:` config use.
    pub dsname: &'static str,
    pub dsmode: DsMode,
    /// `get_instance_id()`.
    pub instance_id: String,
    pub metadata: Object,
    pub userdata_raw: Option<Vec<u8>>,
    pub vendordata_raw: Option<Vec<u8>>,
    pub vendordata2_raw: Option<Vec<u8>>,
    pub network_config: Option<Value>,
    /// `platform_type`.
    pub platform_type: String,
    /// `subplatform` — a `"slug (detail)"` string saying where metadata came
    /// from.
    pub subplatform: String,
    /// `_get_cloud_name()`, before the `cloud-name` metadata key overrides it.
    pub cloud_name_default: String,
    /// The suffix `__str__` appends after the class name, leading separator
    /// included, because upstream emits that separator unconditionally.
    pub detail: String,
}

impl fmt::Display for Datasource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.class_name, self.detail)
    }
}

impl Datasource {
    /// What `Init._reflect_cur_instance` writes to the instance's `datasource`
    /// file: `"%s: %s" % (obj_name(ds), ds)`.
    #[must_use]
    pub fn record(&self) -> String {
        format!("{}: {self}", self.class_name)
    }

    /// `cloud_name` — the metadata key wins, but only when it is a string.
    #[must_use]
    pub fn cloud_name(&self) -> String {
        match self.metadata.get(METADATA_CLOUD_NAME_KEY) {
            Some(Value::String(name)) => name.to_lowercase(),
            _ => self.cloud_name_default.to_lowercase(),
        }
    }

    /// `availability_zone`, falling through to `placement`.
    #[must_use]
    pub fn availability_zone(&self) -> Option<&Value> {
        let top_level = self
            .metadata
            .get("availability-zone")
            .or_else(|| self.metadata.get("availability_zone"));
        match top_level {
            Some(value) if ci_config::option::py_truthy(value) => Some(value),
            _ => self
                .metadata
                .get("placement")
                .and_then(|p| p.get("availability-zone")),
        }
    }

    /// `region`.
    #[must_use]
    pub fn region(&self) -> Option<&Value> {
        self.metadata.get("region")
    }

    /// `get_public_ssh_keys()`.
    ///
    /// The base implementation reads `public-keys`; four of the ported clouds
    /// disagree with it and each other, so this dispatches. GCE is the gap:
    /// its keys live under `public-keys-data` in a format of Google's own,
    /// filtered by expiry and by which account each key names, and none of
    /// that is ported yet — it returns nothing and says so rather than
    /// quietly reading a key upstream would have rejected.
    pub fn public_ssh_keys(&self, log: &mut Logger) -> Vec<String> {
        match self.dsname {
            "Azure" => crate::azure::ds::public_ssh_keys(&self.metadata, log),
            // `SourceMixin` picks the key by metadata version: `public-keys`
            // for v1, `public_keys` for the rest. Only ever one of the two is
            // present, so trying both needs no version to be carried around.
            "OpenStack" | "ConfigDrive" => normalize_pubkey_data(
                self.metadata
                    .get("public_keys")
                    .or_else(|| self.metadata.get("public-keys")),
            ),
            "GCE" => {
                log.log(
                    ci_log::Level::Warning,
                    "DataSourceGCE.py",
                    "Public SSH keys are not read on GCE yet",
                );
                Vec::new()
            }
            _ => normalize_pubkey_data(self.metadata.get("public-keys")),
        }
    }

    /// `canonical_cloud_id(cloud_name, region, platform_type)`.
    #[must_use]
    pub fn cloud_id(&self) -> String {
        let region = self
            .region()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        ci_core::cloud_id::canonical_cloud_id(
            &self.cloud_name(),
            &region,
            &self.platform_type,
        )
    }

    /// `launch_index`.
    #[must_use]
    pub fn launch_index(&self) -> Option<&Value> {
        self.metadata.get("launch-index")
    }
}

/// One `DataSource` subclass: the part of it that runs before there is a
/// [`Datasource`] to hold the answer.
pub trait Probe: fmt::Debug {
    /// `dsname`.
    fn dsname(&self) -> &'static str;

    /// `type_utils.obj_name(cls)`.
    fn class_name(&self) -> &'static str;

    /// `__str__` before `_get_data` has run, which is what the search logs.
    fn display(&self) -> String {
        self.class_name().to_owned()
    }

    /// `ds_detect` — is this cloud present at all? Datasources that only
    /// decide inside `_get_data` leave this alone.
    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        let _ = ctx;
        true
    }

    /// `_get_data`, returning `None` where upstream returns `False`.
    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource>;

    /// `check_instance_id`, which answers from local state only.
    ///
    /// `None` is upstream's `None`: the check could not be made, which callers
    /// treat as a failure to confirm.
    fn check_instance_id(&self, ctx: &mut Context<'_>, current: &str) -> Option<bool> {
        let _ = (ctx, current);
        None
    }

    /// `supported_update_events` — the events this cloud can react to at all.
    fn supported_update_events(&self) -> crate::event::Events {
        crate::event::supported_default()
    }

    /// `default_update_events` — the subset that is on when user-data does not
    /// say otherwise.
    fn default_update_events(&self) -> crate::event::Events {
        crate::event::default_default()
    }

    /// `override_ds_detect` — skip detection when there is nothing to fall back
    /// to, or when the kernel command line named this datasource outright.
    fn override_ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        let named = crate::search::parse_cmdline_or_dmi(ctx.cmdline);
        if self.dsname().eq_ignore_ascii_case(&named) {
            let message = format!(
                "Kernel command line set to use a single datasource {}.",
                self.display()
            );
            ctx.logger.debug("__init__.py", &message);
            return true;
        }
        if ctx.datasource_list() == [self.dsname()] {
            let message = format!(
                "Datasource list set to use a single datasource {}.",
                self.display()
            );
            ctx.logger.debug("__init__.py", &message);
            return true;
        }
        false
    }

    /// `_check_and_get_data`.
    fn check_and_get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        if self.override_ds_detect(ctx) {
            return self.get_data(ctx);
        }
        if self.ds_detect(ctx) {
            ctx.logger
                .debug("__init__.py", &format!("Detected {}", self.display()));
            return self.get_data(ctx);
        }
        ctx.logger
            .debug("__init__.py", &format!("Did not detect {}", self.display()));
        None
    }
}

/// `DataSource.url_timeout`, `url_retries` and `url_sec_between_retries`.
const URL_TIMEOUT_SECS: u64 = 10;
const URL_RETRIES: u32 = 5;
const URL_SEC_BETWEEN_RETRIES: u64 = 1;

/// `DataSource.get_url_params`, narrowed to what the fetcher takes.
///
/// `max_wait` is absent because no caller races URLs yet (deviation 73).
pub(crate) fn url_config(ds_cfg: &Object) -> ci_url::Config {
    let as_u64 = |key: &str, fallback: u64| -> u64 {
        ds_cfg.get(key).and_then(Value::as_u64).unwrap_or(fallback)
    };
    ci_url::Config {
        timeout: std::time::Duration::from_secs(as_u64("timeout", URL_TIMEOUT_SECS)),
        retries: u32::try_from(as_u64("retries", u64::from(URL_RETRIES)))
            .unwrap_or(URL_RETRIES),
        sec_between: std::time::Duration::from_secs(as_u64(
            "sec_between_retries",
            URL_SEC_BETWEEN_RETRIES,
        )),
        ..ci_url::Config::default()
    }
}

/// `sources.convert_vendordata`: a string passes through, a list is joined as
/// a multipart archive, and a mapping is only usable through `cloud-init`.
pub(crate) fn convert_vendordata(
    value: Option<&Value>,
    ctx: &mut Context<'_>,
    source: &str,
    label: &str,
) -> Option<Vec<u8>> {
    match value? {
        Value::Null => None,
        Value::String(text) => Some(text.as_bytes().to_vec()),
        Value::Array(items) => {
            Some(ci_core::json_dumps(&Value::Array(items.clone())).into_bytes())
        }
        Value::Object(map) => map
            .get("cloud-init")
            .and_then(Value::as_str)
            .map(|text| text.as_bytes().to_vec()),
        other => {
            ctx.logger.warning(
                source,
                &format!("Invalid content in {label}: unknown data type {other}"),
            );
            None
        }
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

    fn value(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    #[test]
    fn the_fetch_budget_falls_back_to_the_datasource_defaults() {
        let config = url_config(&Object::new());

        assert_eq!(config.timeout, std::time::Duration::from_secs(10));
        assert_eq!(config.retries, 5);
    }

    #[test]
    fn the_datasource_config_can_override_the_fetch_budget() {
        let mut ds_cfg = Object::new();
        ds_cfg.insert("timeout".to_owned(), serde_json::json!(2));
        ds_cfg.insert("retries".to_owned(), serde_json::json!(0));

        let config = url_config(&ds_cfg);

        assert_eq!(config.timeout, std::time::Duration::from_secs(2));
        assert_eq!(config.retries, 0);
    }

    #[test]
    fn the_network_dsmode_is_spelled_net_on_the_wire() {
        assert_eq!(DsMode::Network.as_str(), "net");
        assert_eq!(DsMode::parse("net"), Some(DsMode::Network));
        assert_eq!(DsMode::parse("network"), None);
    }

    #[test]
    fn the_first_candidate_that_is_set_decides_the_dsmode() {
        let mut logger = Logger::silent();
        let local = value("local");
        let net = value("net");
        assert_eq!(
            DsMode::determine(
                &[None, Some(&local), Some(&net)],
                DsMode::Network,
                &mut logger
            ),
            DsMode::Local
        );
    }

    #[test]
    fn an_unrecognised_dsmode_stops_the_search_rather_than_skipping_it() {
        let mut logger = Logger::silent();
        let bogus = value("nonsense");
        let local = value("local");
        assert_eq!(
            DsMode::determine(
                &[Some(&bogus), Some(&local)],
                DsMode::Network,
                &mut logger
            ),
            DsMode::Network
        );
    }

    #[test]
    fn pass_is_defined_but_never_valid() {
        let mut logger = Logger::silent();
        let pass = value("pass");
        assert_eq!(DsMode::parse("pass"), Some(DsMode::Pass));
        assert_eq!(
            DsMode::determine(&[Some(&pass)], DsMode::Local, &mut logger),
            DsMode::Local
        );
    }

    #[test]
    fn a_non_string_cloud_name_falls_back_to_the_datasource_default() {
        let mut ds = sample();
        ds.metadata
            .insert(METADATA_CLOUD_NAME_KEY.to_owned(), Value::from(7));
        assert_eq!(ds.cloud_name(), "unknown");
        ds.metadata
            .insert(METADATA_CLOUD_NAME_KEY.to_owned(), value("AwS"));
        assert_eq!(ds.cloud_name(), "aws");
    }

    #[test]
    fn an_empty_availability_zone_falls_through_to_placement() {
        let mut ds = sample();
        ds.metadata
            .insert("availability-zone".to_owned(), value(""));
        ds.metadata.insert(
            "placement".to_owned(),
            serde_json::json!({ "availability-zone": "us-east-1a" }),
        );
        assert_eq!(ds.availability_zone(), Some(&value("us-east-1a")));
    }

    #[test]
    fn the_display_form_is_what_gets_written_to_the_datasource_file() {
        let mut ds = sample();
        ds.detail = " [seed=/var/lib/cloud/seed/nocloud]".to_owned();
        assert_eq!(
            ds.record(),
            "DataSourceNoCloud: DataSourceNoCloud [seed=/var/lib/cloud/seed/nocloud]"
        );
    }

    fn sample() -> Datasource {
        Datasource {
            class_name: "DataSourceNoCloud",
            dsname: "NoCloud",
            dsmode: DsMode::Local,
            instance_id: "nocloud".to_owned(),
            metadata: Object::new(),
            userdata_raw: None,
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config: None,
            platform_type: "nocloud".to_owned(),
            subplatform: "seed-dir (/var/lib/cloud/seed/nocloud)".to_owned(),
            cloud_name_default: METADATA_UNKNOWN.to_owned(),
            detail: String::new(),
        }
    }
}
