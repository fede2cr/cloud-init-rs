//! Port of `sources/DataSourceLXD.py`.
//!
//! LXD publishes instance data over an HTTP API on a Unix socket, so the crawl
//! is a handful of `GET`s against `http://lxd/1.0/...` spoken down
//! `/dev/lxd/sock`. The socket path is a parameter rather than a constant so
//! the differential can point both implementations at the same test server.

use std::path::{Path, PathBuf};

use ci_config::{Object, Value};

use crate::types::{Context, Datasource, DsMode, Probe, METADATA_UNKNOWN};

/// `LXD_SOCKET_PATH`.
pub const SOCKET_PATH: &str = "/dev/lxd/sock";
/// `LXD_SOCKET_API_VERSION`.
const API_VERSION: &str = "1.0";
/// `LXD_URL`. The host is a placeholder; the socket decides where it goes.
const LXD_URL: &str = "http://lxd";

/// `CONFIG_KEY_ALIASES`, as `(config key, top-level alias)`.
const CONFIG_KEY_ALIASES: &[(&str, &str)] = &[
    ("cloud-init.user-data", "user-data"),
    ("cloud-init.network-config", "network-config"),
    ("cloud-init.vendor-data", "vendor-data"),
    ("user.user-data", "user-data"),
    ("user.network-config", "network-config"),
    ("user.vendor-data", "vendor-data"),
];

/// `_do_request` retries a 500 this many times, sleeping between each.
const RETRIES_ON_500: u32 = 30;
const SLEEP_ON_500: std::time::Duration = std::time::Duration::from_millis(100);

/// `MetaDataKeys`, as the two combinations upstream actually asks for.
#[derive(Debug, Clone, Copy)]
pub struct Keys {
    pub meta_data: bool,
    pub config: bool,
    pub devices: bool,
}

impl Keys {
    /// `MetaDataKeys.ALL`.
    #[must_use]
    pub fn all() -> Self {
        Self {
            meta_data: true,
            config: true,
            devices: true,
        }
    }

    /// `MetaDataKeys.META_DATA`.
    #[must_use]
    pub fn meta_data() -> Self {
        Self {
            meta_data: true,
            config: false,
            devices: false,
        }
    }
}

fn config_for(socket: &Path) -> ci_url::Config {
    ci_url::Config {
        // The 500 retry below is upstream's own loop; `readurl`'s would also
        // retry a 404, which upstream answers immediately.
        retries: 0,
        sec_between: std::time::Duration::ZERO,
        unix_socket: Some(socket.to_path_buf()),
        ..ci_url::Config::default()
    }
}

/// `_do_request`: a `GET` that treats a 500 as worth waiting out.
fn do_request(
    socket: &Path,
    url: &str,
    do_raise: bool,
    log: &mut ci_log::Logger,
) -> Result<ci_url::Response, String> {
    let config = config_for(socket);
    let mut last = Err(String::new());
    for remaining in (1..=RETRIES_ON_500).rev() {
        last = ci_url::readurl(url, &config).or_else(|e| match e.code {
            // `readurl` reports a non-ok response as an error; upstream's
            // `session.get` hands the response back either way.
            Some(code) => Ok(ci_url::Response {
                url: e.url.clone(),
                code,
                headers: Vec::new(),
                contents: Vec::new(),
            }),
            None => Err(e.message),
        });
        match &last {
            Ok(response) if response.code == 500 => {
                log.warning(
                    "DataSourceLXD.py",
                    &format!(
                        "[GET] [HTTP:500] {url}, retrying {remaining} more time(s)"
                    ),
                );
                std::thread::sleep(SLEEP_ON_500);
            }
            _ => break,
        }
    }

    let response = last?;
    log.debug(
        "DataSourceLXD.py",
        &format!("[GET] [HTTP:{}] {url}", response.code),
    );
    if do_raise && !response.ok() {
        return Err(format!(
            "Invalid HTTP response [{}] from {url}: {}",
            response.code,
            String::from_utf8_lossy(&response.contents)
        ));
    }
    Ok(response)
}

/// `_get_json_response`.
fn get_json_response(
    socket: &Path,
    url: &str,
    do_raise: bool,
    log: &mut ci_log::Logger,
) -> Result<Value, String> {
    let response = do_request(socket, url, do_raise, log)?;
    let text = String::from_utf8_lossy(&response.contents).into_owned();
    if !response.ok() {
        log.debug(
            "DataSourceLXD.py",
            &format!("Skipping {url} on [HTTP:{}]:{text}", response.code),
        );
        return Ok(Value::Object(Object::new()));
    }
    serde_json::from_str(&text).map_err(|_| {
        format!(
            "Unable to process LXD config at {url}. Expected JSON but found: {text}"
        )
    })
}

/// `_MetaDataReader._process_config`, writing into `md` the way `md.update`
/// does upstream.
fn process_config(
    socket: &Path,
    md: &mut Object,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    let version_url = format!("{LXD_URL}/{API_VERSION}");
    let routes =
        get_json_response(socket, &format!("{version_url}/config"), true, log)?;
    let mut config = Object::new();
    let mut promoted: Vec<&str> = Vec::new();

    // Sorted so `cloud-init.*` is seen before the `user.*` it overrides.
    let mut routes: Vec<String> = routes
        .as_array()
        .map(|routes| {
            routes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    routes.sort();

    for route in routes {
        let route_url = format!("{LXD_URL}{route}");
        let response = do_request(socket, &route_url, false, log)?;
        let text = String::from_utf8_lossy(&response.contents).into_owned();
        if !response.ok() {
            log.debug(
                "DataSourceLXD.py",
                &format!("Skipping {route_url} on [HTTP:{}]:{text}", response.code),
            );
            continue;
        }
        let cfg_key = route.rsplit('/').next().unwrap_or(&route).to_owned();
        let Some((_, alias)) =
            CONFIG_KEY_ALIASES.iter().find(|(key, _)| *key == cfg_key)
        else {
            config.insert(cfg_key, Value::String(text));
            continue;
        };
        if promoted.contains(alias) {
            log.warning(
                "DataSourceLXD.py",
                &format!(
                    "Ignoring LXD config {cfg_key} in favor of {} value.",
                    cfg_key.replacen("user", "cloud-init", 1)
                ),
            );
        } else {
            promoted.push(alias);
            md.insert((*alias).to_owned(), Value::String(text.clone()));
        }
        config.insert(cfg_key, Value::String(text));
    }

    md.insert("config".to_owned(), Value::Object(config));
    Ok(())
}

/// `read_metadata`.
///
/// # Errors
/// The socket is unreachable, `meta-data` does not answer, or a document that
/// must be JSON is not.
pub fn read_metadata(
    socket: &Path,
    keys: Keys,
    log: &mut ci_log::Logger,
) -> Result<Object, String> {
    let version_url = format!("{LXD_URL}/{API_VERSION}");
    let mut md = Object::new();
    md.insert(
        "_metadata_api_version".to_owned(),
        Value::String(API_VERSION.to_owned()),
    );

    if keys.meta_data {
        let response =
            do_request(socket, &format!("{version_url}/meta-data"), true, log)?;
        md.insert(
            "meta-data".to_owned(),
            Value::String(String::from_utf8_lossy(&response.contents).into_owned()),
        );
    }
    if keys.config {
        process_config(socket, &mut md, log)?;
    }
    if keys.devices {
        let devices =
            get_json_response(socket, &format!("{version_url}/devices"), false, log)?;
        if !is_blank(&devices) {
            md.insert("devices".to_owned(), devices);
        }
    }
    Ok(md)
}

/// `_raw_instance_data_to_dict`.
///
/// `util.load_yaml`'s default `allowed=(dict,)` means anything whose root is
/// not a mapping — including an empty document and a parse failure — comes
/// back as `None`, which upstream turns into an error.
fn raw_instance_data_to_dict(
    md_type: &str,
    value: Option<&Value>,
    limits: ci_config::Limits,
) -> Result<Object, String> {
    match value {
        None | Some(Value::Null) => Ok(Object::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(Value::String(text)) => match ci_config::load_yaml(text, limits) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(format!(
                "Invalid {md_type} format. Expected YAML but found: {text}"
            )),
        },
        Some(other) => Err(format!(
            "Invalid {md_type}. Expected str, bytes or dict but found: {other}"
        )),
    }
}

#[derive(Debug)]
pub struct Lxd;

impl Probe for Lxd {
    fn dsname(&self) -> &'static str {
        "LXD"
    }

    fn class_name(&self) -> &'static str {
        "DataSourceLXD"
    }

    fn ds_detect(&self, ctx: &mut Context<'_>) -> bool {
        let path = socket_path(ctx);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            ctx.logger.warning(
                "DataSourceLXD.py",
                &format!("{} does not exist.", path.display()),
            );
            return false;
        };
        if !std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()) {
            ctx.logger.warning(
                "DataSourceLXD.py",
                &format!("{} is not a socket", path.display()),
            );
            return false;
        }
        true
    }

    fn get_data(&self, ctx: &mut Context<'_>) -> Option<Datasource> {
        let path = socket_path(ctx);
        let crawled = match read_metadata(&path, Keys::all(), ctx.logger) {
            Ok(crawled) => crawled,
            Err(reason) => {
                ctx.logger.error("DataSourceLXD.py", &reason);
                return None;
            }
        };
        build(&crawled, &path, ctx.limits, ctx.logger)
    }

    fn check_instance_id(&self, ctx: &mut Context<'_>, current: &str) -> Option<bool> {
        let path = socket_path(ctx);
        let limits = ctx.limits;
        let crawled = read_metadata(&path, Keys::meta_data(), ctx.logger).ok()?;
        let md =
            raw_instance_data_to_dict("meta-data", crawled.get("meta-data"), limits)
                .ok()?;
        Some(md.get("instance-id").and_then(Value::as_str) == Some(current))
    }
}

/// `_get_data`'s half that turns a crawl into a datasource.
fn build(
    crawled: &Object,
    socket: &Path,
    limits: ci_config::Limits,
    log: &mut ci_log::Logger,
) -> Option<Datasource> {
    let mut metadata = match raw_instance_data_to_dict(
        "meta-data",
        crawled.get("meta-data"),
        limits,
    ) {
        Ok(metadata) => metadata,
        Err(reason) => {
            log.error("DataSourceLXD.py", &reason);
            return None;
        }
    };
    let user_metadata = crawled
        .get("config")
        .and_then(Value::as_object)
        .and_then(|config| config.get("user.meta-data"));
    if user_metadata.is_some_and(|value| !is_blank(value)) {
        match raw_instance_data_to_dict("user.meta-data", user_metadata, limits) {
            Ok(extra) => metadata.extend(extra),
            // As with network-config below, upstream throws the whole
            // datasource away over one bad document (bug B44).
            Err(reason) => log.warning("DataSourceLXD.py", &reason),
        }
    }

    let as_bytes = |key: &str| {
        crawled
            .get(key)
            .and_then(Value::as_str)
            .map(|text| text.as_bytes().to_vec())
    };
    let instance_id = metadata
        .get("instance-id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let network_config = crawled
        .get("network-config")
        .and_then(Value::as_str)
        .map(|text| ci_config::load_yaml(text, limits));
    let network_config = match network_config {
        Some(Ok(value @ Value::Object(_))) => Some(value),
        // Upstream raises here, which throws the whole datasource away
        // along with the user-data it had already read (bug B44).
        Some(_) => {
            log.warning(
                "DataSourceLXD.py",
                "Ignoring network-config: expected a YAML mapping.",
            );
            None
        }
        None => None,
    };

    Some(Datasource {
        class_name: "DataSourceLXD",
        dsname: "LXD",
        dsmode: DsMode::Network,
        instance_id,
        platform_type: "lxd".to_owned(),
        subplatform: format!("LXD socket API v. {API_VERSION} ({})", socket.display()),
        cloud_name_default: METADATA_UNKNOWN.to_owned(),
        detail: String::new(),
        metadata,
        userdata_raw: as_bytes("user-data"),
        vendordata_raw: as_bytes("vendor-data"),
        vendordata2_raw: None,
        // Offered raw; the fallback that guesses a NIC when LXD supplies
        // none needs the network layer (deviation 76).
        network_config,
    })
}

/// Python's `if value:` over a JSON document.
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// The socket to crawl. Upstream has only the constant; the port lets the
/// datasource config move it so the crawl can be exercised without LXD.
fn socket_path(ctx: &mut Context<'_>) -> PathBuf {
    ctx.ds_cfg("LXD")
        .get("sock_path")
        .and_then(Value::as_str)
        .map_or_else(|| PathBuf::from(SOCKET_PATH), PathBuf::from)
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
        serde_json::from_str(text).unwrap()
    }

    fn parse(md_type: &str, raw: &str) -> Result<Object, String> {
        raw_instance_data_to_dict(
            md_type,
            Some(&value(raw)),
            ci_config::Limits::default(),
        )
    }

    #[test]
    fn meta_data_is_parsed_from_yaml_and_a_dict_is_taken_as_is() {
        let parsed = parse("meta-data", r#""instance-id: i-1""#).unwrap();
        assert_eq!(parsed.get("instance-id").unwrap(), "i-1");

        let parsed = parse("meta-data", r#"{"instance-id": "i-2"}"#).unwrap();
        assert_eq!(parsed.get("instance-id").unwrap(), "i-2");

        assert!(raw_instance_data_to_dict(
            "meta-data",
            None,
            ci_config::Limits::default()
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn meta_data_whose_root_is_not_a_mapping_is_refused() {
        for raw in [r#""""#, r#""just a string""#, r#""- a\n- b\n""#] {
            let error = parse("meta-data", raw).unwrap_err();
            assert!(error.contains("Expected YAML but found"), "{raw}: {error}");
        }
    }

    #[test]
    fn a_cloud_init_config_key_wins_over_the_user_one() {
        // The precedence is carried entirely by sorting the routes, so this
        // pins the order the promotion loop depends on.
        let mut routes = [
            "/1.0/config/user.user-data",
            "/1.0/config/cloud-init.user-data",
        ];
        routes.sort_unstable();
        assert_eq!(routes[0], "/1.0/config/cloud-init.user-data");

        let aliased: Vec<&str> = CONFIG_KEY_ALIASES
            .iter()
            .filter(|(_, alias)| *alias == "user-data")
            .map(|(key, _)| *key)
            .collect();
        assert_eq!(aliased, ["cloud-init.user-data", "user.user-data"]);
    }

    #[test]
    fn the_overridden_key_is_named_in_the_warning_it_earns() {
        assert_eq!(
            "user.user-data".replacen("user", "cloud-init", 1),
            "cloud-init.user-data"
        );
        assert_eq!(
            "user.network-config".replacen("user", "cloud-init", 1),
            "cloud-init.network-config"
        );
    }

    #[test]
    fn user_meta_data_is_merged_over_the_meta_data_document() {
        let mut crawled = Object::new();
        crawled.insert(
            "meta-data".to_owned(),
            Value::String("instance-id: i-1\nlocal-hostname: a\n".to_owned()),
        );
        let mut config = Object::new();
        config.insert(
            "user.meta-data".to_owned(),
            Value::String("local-hostname: b\n".to_owned()),
        );
        crawled.insert("config".to_owned(), Value::Object(config));

        let mut log = ci_log::Logger::silent();
        let ds = build(
            &crawled,
            Path::new("/dev/lxd/sock"),
            ci_config::Limits::default(),
            &mut log,
        )
        .unwrap();
        assert_eq!(ds.instance_id, "i-1");
        assert_eq!(ds.metadata.get("local-hostname").unwrap(), "b");
        assert_eq!(ds.subplatform, "LXD socket API v. 1.0 (/dev/lxd/sock)");
    }

    #[test]
    fn a_network_config_that_is_not_a_mapping_is_dropped_not_fatal() {
        let mut crawled = Object::new();
        crawled.insert(
            "meta-data".to_owned(),
            Value::String("instance-id: i-1\n".to_owned()),
        );
        crawled.insert(
            "user-data".to_owned(),
            Value::String("#cloud-config\n".to_owned()),
        );
        crawled.insert("network-config".to_owned(), Value::String(String::new()));

        let mut log = ci_log::Logger::silent();
        let ds = build(
            &crawled,
            Path::new("/dev/lxd/sock"),
            ci_config::Limits::default(),
            &mut log,
        )
        .expect("upstream discards the whole datasource here");
        assert!(ds.network_config.is_none());
        assert_eq!(ds.userdata_raw.unwrap(), b"#cloud-config\n");
    }

    #[test]
    fn a_missing_socket_is_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("sock");
        assert!(std::fs::symlink_metadata(&missing).is_err());
        // A plain file at the path is refused too.
        std::fs::write(&missing, b"").unwrap();
        let meta = std::fs::symlink_metadata(&missing).unwrap();
        assert!(!std::os::unix::fs::FileTypeExt::is_socket(
            &meta.file_type()
        ));
    }
}
