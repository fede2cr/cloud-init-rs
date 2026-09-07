//! Port of `sources/DataSourceEc2.py`.
//!
//! The network variant only: `DataSourceEc2Local` sorts candidate NICs by
//! driver and takes an ephemeral DHCP lease on each, which belongs with the
//! network layer (deviation 75).

use std::time::{Duration, Instant};

use ci_config::{Object, Value};

use crate::helpers::ec2 as helper;
use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `CloudNames`.
mod cloud_names {
    pub(super) const AWS: &str = "aws";
    pub(super) const BRIGHTBOX: &str = "brightbox";
    pub(super) const ZSTACK: &str = "zstack";
    pub(super) const E24CLOUD: &str = "e24cloud";
    pub(super) const OUTSCALE: &str = "outscale";
    pub(super) const TILAA: &str = "tilaa";
    /// No positive identification; the metadata service is still tried unless
    /// `strict_id` says otherwise.
    pub(super) const UNKNOWN: &str = "unknown";
}

/// `DataSourceEc2.metadata_urls`.
const METADATA_URLS: &[&str] = &["http://169.254.169.254", "http://[fd00:ec2::254]"];

/// `min_metadata_version` and `extended_metadata_versions`.
const MIN_METADATA_VERSION: &str = "2009-04-04";
const EXTENDED_METADATA_VERSIONS: &[&str] = &["2021-03-23", "2018-09-24", "2016-09-02"];

/// `url_max_wait` and `url_timeout`. `url_retries` comes from the base class.
const URL_MAX_WAIT_SECS: u64 = 240;
const URL_TIMEOUT_SECS: u64 = 50;

/// `STRICT_ID_PATH` / `STRICT_ID_DEFAULT`.
const STRICT_ID_DEFAULT: &str = "warn";

/// The `IMDSv2` token exchange.
const API_TOKEN_ROUTE: &str = "latest/api/token";
const TOKEN_TTL_SECONDS: &str = "21600";
const TOKEN_PUT_HEADER: &str = "X-aws-ec2-metadata-token";
const TOKEN_REQ_HEADER: &str = "X-aws-ec2-metadata-token-ttl-seconds";

/// `IDMSV2_SUPPORTED_CLOUD_PLATFORMS`.
fn supports_imdsv2(cloud_name: &str) -> bool {
    cloud_name == cloud_names::AWS
}

/// `parse_strict_mode`, returning `None` where upstream raises `ValueError`.
///
/// The sleep is parsed and discarded: it only paces the `non_ec2_md` warning,
/// which needs the `warnings` module (deviation 75).
fn parse_strict_mode(cfgval: &Value) -> Option<&'static str> {
    match cfgval {
        Value::Bool(true) => return Some("true"),
        Value::Bool(false) => return Some("false"),
        Value::Null => return Some("warn"),
        _ => {}
    }
    let text = cfgval.as_str()?;
    if text.is_empty() {
        return Some("warn");
    }
    let (mode, sleep) = text.split_once(',').unwrap_or((text, ""));
    if !sleep.is_empty() && sleep.parse::<i64>().is_err() {
        return None;
    }
    match mode {
        "true" => Some("true"),
        "false" => Some("false"),
        "warn" => Some("warn"),
        _ => None,
    }
}

/// `util.maybe_b64decode`, which decodes only strictly valid base64.
///
/// `b64decode(validate=True)` rejects anything outside the alphabet, including
/// the whitespace [`ci_core::b64::decode`] tolerates, so the alphabet and the
/// length are checked here before handing the text over.
#[must_use]
pub fn maybe_b64decode(data: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(data) else {
        return data.to_vec();
    };
    let alphabet = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'/';
    let body = text.trim_end_matches('=');
    if text.is_empty()
        || text.len() % 4 != 0
        || text.len() - body.len() > 2
        || !body.bytes().all(alphabet)
    {
        return data.to_vec();
    }
    ci_core::b64::decode(text).unwrap_or_else(|| data.to_vec())
}

/// `_collect_platform_data`, lower-cased as upstream leaves it.
#[derive(Debug, Default)]
struct PlatformData {
    uuid: String,
    serial: String,
    asset_tag: String,
    vendor: String,
    product_name: String,
}

fn collect_platform_data(ctx: &mut Context<'_>) -> PlatformData {
    let dmi = |key: &str, ctx: &mut Context<'_>| {
        crate::dmi::read_dmi_data(key, ctx.logger)
            .unwrap_or_default()
            .to_lowercase()
    };
    // The Xen hypervisor exposes the instance UUID before DMI does.
    let uuid = std::fs::read_to_string("/sys/hypervisor/uuid")
        .ok()
        .map(|text| text.trim().to_lowercase())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| dmi("system-uuid", ctx));
    PlatformData {
        uuid,
        serial: dmi("system-serial-number", ctx),
        asset_tag: dmi("chassis-asset-tag", ctx),
        vendor: dmi("system-manufacturer", ctx),
        product_name: dmi("system-product-name", ctx),
    }
}

/// `identify_aws`: the UUID begins `ec2` in either endianness.
fn identify_aws(uuid: &str) -> bool {
    if uuid.starts_with("ec2") {
        return true;
    }
    let hex: String = uuid
        .chars()
        .filter(|c| *c != '-')
        .take_while(char::is_ascii_hexdigit)
        .collect();
    if hex.len() != 32 {
        return false;
    }
    // `UUID.bytes_le` reverses the first field, so its first three nibbles are
    // the last byte of that field followed by the high nibble of the previous.
    let byte = |at: usize| hex.get(at..at + 2).unwrap_or_default();
    format!("{}{}", byte(6), byte(4)).starts_with("ec2")
}

/// `identify_platform`.
fn identify_platform(data: &PlatformData) -> &'static str {
    if identify_aws(&data.uuid) {
        cloud_names::AWS
    } else if data.serial.ends_with(".brightbox.com") {
        cloud_names::BRIGHTBOX
    } else if data.asset_tag.ends_with(".zstack.io") {
        cloud_names::ZSTACK
    } else if data.vendor == "e24cloud" {
        cloud_names::E24CLOUD
    } else if data.product_name == "3ds outscale vm" && data.vendor == "3ds outscale" {
        cloud_names::OUTSCALE
    } else if data.vendor == "tilaa" {
        cloud_names::TILAA
    } else {
        cloud_names::UNKNOWN
    }
}

/// The bound `read_file_or_url` upstream builds from `headers_cb`.
///
/// The token is refreshed lazily, and dropped on a 401 so the next request
/// fetches a fresh one — upstream's `_refresh_stale_aws_token_cb`.
struct Fetcher<'a> {
    address: String,
    config: ci_url::Config,
    imdsv2: bool,
    token: Option<String>,
    logger: &'a mut ci_log::Logger,
}

impl Fetcher<'_> {
    /// `_refresh_api_token`.
    fn refresh_token(&mut self) -> Option<String> {
        let token_url = format!("{}/{API_TOKEN_ROUTE}", self.address);
        let config = ci_url::Config {
            method: ci_url::Method::Put,
            headers: vec![(TOKEN_REQ_HEADER.to_owned(), TOKEN_TTL_SECONDS.to_owned())],
            ..self.config.clone()
        };
        match ci_url::readurl(&token_url, &config) {
            Ok(response) => {
                Some(String::from_utf8_lossy(&response.contents).into_owned())
            }
            Err(e) => {
                self.logger.warning(
                    "DataSourceEc2.py",
                    &format!(
                        "Unable to get API token: {token_url} raised exception {e}"
                    ),
                );
                None
            }
        }
    }

    /// `_get_headers`.
    fn headers(&mut self) -> Vec<(String, String)> {
        if !self.imdsv2 {
            return Vec::new();
        }
        if self.token.is_none() {
            self.token = self.refresh_token();
        }
        self.token
            .as_ref()
            .map(|token| vec![(TOKEN_PUT_HEADER.to_owned(), token.clone())])
            .unwrap_or_default()
    }
}

impl helper::Caller for Fetcher<'_> {
    fn fetch(&mut self, url: &str) -> Result<Vec<u8>, ci_url::Error> {
        let config = ci_url::Config {
            headers: self.headers(),
            ..self.config.clone()
        };
        match ci_url::readurl(url, &config) {
            Ok(response) => Ok(response.contents),
            Err(e) => {
                if e.code == Some(401) {
                    self.logger.debug(
                        "DataSourceEc2.py",
                        "Clearing cached Ec2 API token due to expiry",
                    );
                    self.token = None;
                }
                Err(e)
            }
        }
    }
}

#[derive(Debug)]
pub struct Ec2;

impl Ec2 {
    /// `mcfg.get("metadata_urls", self.metadata_urls)`.
    fn metadata_urls(ds_cfg: &Object) -> Vec<String> {
        ds_cfg
            .get("metadata_urls")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|url| url.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .filter(|urls: &Vec<String>| !urls.is_empty())
            .unwrap_or_else(|| METADATA_URLS.iter().map(|s| (*s).to_owned()).collect())
    }

    /// `wait_for_metadata_service`, without `is_resolvable_url` and without
    /// `wait_for_url`'s concurrent connect (deviation 75).
    ///
    /// The `IMDSv2` token request doubles as the probe on AWS, exactly as it does
    /// upstream: a platform that answers a `PUT` for a token is the platform
    /// the metadata is then read from.
    fn wait_for_metadata_service(
        ds_cfg: &Object,
        cloud_name: &str,
        config: &ci_url::Config,
        max_wait: Duration,
        ctx: &mut Context<'_>,
    ) -> Option<(String, Option<String>)> {
        if max_wait.is_zero() {
            return None;
        }
        let urls = Self::metadata_urls(ds_cfg);
        let imdsv2 = supports_imdsv2(cloud_name);
        let deadline = Instant::now() + max_wait;

        loop {
            for base in &urls {
                if imdsv2 {
                    let token_url = format!("{base}/{API_TOKEN_ROUTE}");
                    let probe = ci_url::Config {
                        method: ci_url::Method::Put,
                        headers: vec![(
                            TOKEN_REQ_HEADER.to_owned(),
                            TOKEN_TTL_SECONDS.to_owned(),
                        )],
                        retries: 0,
                        ..config.clone()
                    };
                    match ci_url::readurl(&token_url, &probe) {
                        Ok(response) => {
                            let token = String::from_utf8_lossy(&response.contents)
                                .into_owned();
                            return Some((base.clone(), Some(token)));
                        }
                        // Amazon's guidance is that a 4xx on the token route
                        // will not be fixed by retrying.
                        Err(e)
                            if e.code
                                .is_some_and(|code| (400..500).contains(&code)) =>
                        {
                            ctx.logger.warning(
                                "DataSourceEc2.py",
                                "Fatal error while requesting Ec2 IMDSv2 API tokens",
                            );
                            return None;
                        }
                        Err(_) => {}
                    }
                } else {
                    let probe =
                        format!("{base}/{MIN_METADATA_VERSION}/meta-data/instance-id");
                    let config = ci_url::Config {
                        retries: 0,
                        ..config.clone()
                    };
                    if ci_url::readurl(&probe, &config).is_ok_and(|r| r.ok()) {
                        return Some((base.clone(), None));
                    }
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_secs(1).min(config.sec_between));
        }

        if imdsv2 {
            ctx.logger.warning(
                "DataSourceEc2.py",
                "IMDS's HTTP endpoint is probably disabled",
            );
        } else {
            ctx.logger.error(
                "DataSourceEc2.py",
                &format!(
                    "Giving up on md from {urls:?} after {} seconds",
                    max_wait.as_secs()
                ),
            );
        }
        None
    }

    /// `get_metadata_api_version`.
    fn metadata_api_version(fetcher: &mut Fetcher<'_>) -> String {
        for version in EXTENDED_METADATA_VERSIONS {
            let url = format!("{}/{version}/meta-data/instance-id", fetcher.address);
            let headers = fetcher.headers();
            let config = ci_url::Config {
                headers,
                retries: 0,
                ..fetcher.config.clone()
            };
            if ci_url::readurl(&url, &config).is_ok_and(|response| response.code == 200)
            {
                fetcher.logger.debug(
                    "DataSourceEc2.py",
                    &format!("Found preferred metadata version {version}"),
                );
                return (*version).to_owned();
            }
        }
        MIN_METADATA_VERSION.to_owned()
    }
}

/// `crawl_metadata`, minus `wait_for_metadata_service`, which the caller has
/// already run so it can report the address it settled on.
#[derive(Debug, Default)]
pub struct Crawled {
    pub api_version: String,
    pub metadata: Object,
    pub userdata: Vec<u8>,
    /// `dynamic.instance-identity`, crawled on AWS only.
    pub identity: Object,
}

/// The metadata half of `crawl_metadata`, callable without a [`Context`].
#[must_use]
pub fn crawl(
    address: &str,
    cloud_name: &str,
    config: &ci_url::Config,
    token: Option<String>,
    logger: &mut ci_log::Logger,
) -> Crawled {
    let mut fetcher = Fetcher {
        address: address.to_owned(),
        config: config.clone(),
        imdsv2: supports_imdsv2(cloud_name),
        token,
        logger,
    };
    let api_version = Ec2::metadata_api_version(&mut fetcher);

    let raw_userdata = helper::instance_userdata(
        &api_version,
        address,
        &mut fetcher,
        &mut ci_log::Logger::silent(),
    );
    let mut log = ci_log::Logger::silent();
    let metadata =
        helper::instance_metadata(&api_version, address, &mut fetcher, &mut log);
    let identity = if supports_imdsv2(cloud_name) {
        helper::instance_identity(&api_version, address, &mut fetcher, &mut log)
    } else {
        Object::new()
    };

    Crawled {
        api_version,
        metadata,
        userdata: maybe_b64decode(&raw_userdata),
        identity,
    }
}

impl Probe for Ec2 {
    fn dsname(&self) -> &'static str {
        "Ec2"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceEc2"
    }

    /// Upstream also adds `BOOT` and `BOOT_LEGACY` here from `network_config`
    /// when the metadata carries a network document, which is Phase 4.
    fn default_update_events(&self) -> crate::event::Events {
        crate::event::events(
            crate::event::Scope::Network,
            &[
                crate::event::Type::BootNewInstance,
                crate::event::Type::Hotplug,
            ],
        )
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let ds_cfg = ctx.ds_cfg(self.dsname());
        let strict_id = ds_cfg
            .get("strict_id")
            .cloned()
            .unwrap_or_else(|| Value::String(STRICT_ID_DEFAULT.to_owned()));
        let strict_mode = parse_strict_mode(&strict_id).unwrap_or_else(|| {
            ctx.logger.warning(
                "DataSourceEc2.py",
                &format!("Invalid mode in strict_id setting '{strict_id}'"),
            );
            "warn"
        });

        let platform_data = collect_platform_data(ctx);
        let cloud_name = identify_platform(&platform_data);
        ctx.logger.debug(
            "DataSourceEc2.py",
            &format!("strict_mode: {strict_mode}, cloud_name={cloud_name} cloud_platform=ec2"),
        );
        if strict_mode == "true" && cloud_name == cloud_names::UNKNOWN {
            return None;
        }

        let mut config = crate::types::url_config(&ds_cfg);
        config.timeout = Duration::from_secs(
            ds_cfg
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(URL_TIMEOUT_SECS),
        );
        let max_wait = Duration::from_secs(
            ds_cfg
                .get("max_wait")
                .and_then(Value::as_u64)
                .unwrap_or(URL_MAX_WAIT_SECS),
        );

        let (address, token) = Self::wait_for_metadata_service(
            &ds_cfg, cloud_name, &config, max_wait, ctx,
        )?;
        ctx.logger.debug(
            "DataSourceEc2.py",
            &format!("Using metadata source: '{address}'"),
        );

        let crawled = crawl(&address, cloud_name, &config, token, ctx.logger);
        if crawled.metadata.is_empty() {
            // Upstream reports success here and then raises KeyError out of
            // `get_instance_id`, aborting the search (bug B43).
            ctx.logger
                .error("DataSourceEc2.py", "Unable to get metadata");
            return None;
        }

        let mut metadata = crawled.metadata;
        // `get_instance_id` prefers the identity document on AWS.
        let instance_id = crawled
            .identity
            .get("document")
            .and_then(|doc| doc.get("instanceId"))
            .or_else(|| metadata.get("instance-id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // `availability_zone` and `region` read these off the identity
        // document, which the port folds into metadata rather than keeping a
        // second copy of the crawl.
        if let Some(document) = crawled.identity.get("document") {
            if let Some(region) = document.get("region") {
                metadata.insert("region".to_owned(), region.clone());
            }
            if let Some(zone) = document.get("availabilityZone") {
                metadata.insert("availability-zone".to_owned(), zone.clone());
            }
        }

        let userdata_raw = (!crawled.userdata.is_empty()).then_some(crawled.userdata);
        Some(Datasource {
            class_name: self.class_name(),
            dsname: self.dsname(),
            dsmode: DsMode::Network,
            instance_id,
            metadata,
            userdata_raw,
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config: None,
            platform_type: "ec2".to_owned(),
            subplatform: format!("metadata ({address})"),
            cloud_name_default: if cloud_name == cloud_names::UNKNOWN {
                METADATA_UNKNOWN.to_owned()
            } else {
                cloud_name.to_owned()
            },
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

    #[test]
    fn an_ec2_uuid_is_recognised_in_either_endianness() {
        assert!(identify_aws("ec2e1916-9099-7caf-fd21-012345abcdef"));
        assert!(identify_aws("45e12aec-dcd1-b213-94ed-012345abcdef"));
        assert!(!identify_aws("12345678-9099-7caf-fd21-012345abcdef"));
        assert!(!identify_aws("not-a-uuid"));
    }

    #[test]
    fn the_other_clouds_are_identified_by_their_dmi_strings() {
        let named = |data: PlatformData| identify_platform(&data);

        assert_eq!(
            named(PlatformData {
                serial: "srv-1.gb1.brightbox.com".to_owned(),
                ..PlatformData::default()
            }),
            cloud_names::BRIGHTBOX
        );
        assert_eq!(
            named(PlatformData {
                asset_tag: "123.zstack.io".to_owned(),
                ..PlatformData::default()
            }),
            cloud_names::ZSTACK
        );
        assert_eq!(
            named(PlatformData {
                product_name: "3ds outscale vm".to_owned(),
                vendor: "3ds outscale".to_owned(),
                ..PlatformData::default()
            }),
            cloud_names::OUTSCALE
        );
        assert_eq!(named(PlatformData::default()), cloud_names::UNKNOWN);
    }

    #[test]
    fn strict_id_accepts_a_mode_with_an_optional_sleep() {
        assert_eq!(
            parse_strict_mode(&Value::String("warn".into())),
            Some("warn")
        );
        assert_eq!(
            parse_strict_mode(&Value::String("warn,30".into())),
            Some("warn")
        );
        assert_eq!(parse_strict_mode(&Value::Bool(true)), Some("true"));
        assert_eq!(
            parse_strict_mode(&Value::String(String::new())),
            Some("warn")
        );
        assert_eq!(parse_strict_mode(&Value::String("maybe".into())), None);
        assert_eq!(parse_strict_mode(&Value::String("warn,soon".into())), None);
    }

    #[test]
    fn user_data_is_decoded_only_when_it_is_really_base64() {
        assert_eq!(maybe_b64decode(b"I2Nsb3VkLWNvbmZpZwo="), b"#cloud-config\n");
        assert_eq!(maybe_b64decode(b"#cloud-config\n"), b"#cloud-config\n");
        // Valid base64 characters, but the wrong length to be base64.
        assert_eq!(maybe_b64decode(b"hello"), b"hello");
        // Whitespace is outside the alphabet `validate=True` accepts.
        assert_eq!(maybe_b64decode(b"aGVs bG8="), b"aGVs bG8=");
    }

    #[test]
    fn the_default_metadata_urls_are_used_when_the_config_names_none() {
        assert_eq!(Ec2::metadata_urls(&Object::new()), METADATA_URLS);

        let mut ds_cfg = Object::new();
        ds_cfg.insert("metadata_urls".to_owned(), serde_json::json!(["http://a"]));
        assert_eq!(Ec2::metadata_urls(&ds_cfg), vec!["http://a".to_owned()]);
    }
}
