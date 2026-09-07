//! Port of `sources/helpers/openstack.py`.
//!
//! Upstream shares one `BaseReader` between the config drive and the network
//! metadata service, with the filesystem and HTTP differences pushed into
//! abstract methods. [`Source`] is that seam: [`read_v2`] is written once and
//! reads through it, while `read_v1` is config-drive only, as upstream's is.

use std::path::Path;

use ci_config::{Object, Value};

/// `openstack.OS_VERSIONS`, chronological as upstream keeps it.
const OS_VERSIONS: &[&str] = &[
    "2012-08-10",
    "2013-04-04",
    "2013-10-17",
    "2015-10-15",
    "2016-06-30",
    "2016-10-06",
    "2017-02-22",
    "2018-08-27",
];

/// `openstack.OS_LATEST`.
const OS_LATEST: &str = "latest";

/// `openstack.KEY_COPIES`: metadata renames applied after a v2 read, with the
/// flag saying whether the source key has to be there.
const KEY_COPIES: &[(&str, &str, bool)] = &[
    ("local-hostname", "hostname", false),
    ("instance-id", "uuid", true),
];

/// Why a source directory yielded nothing.
///
/// Upstream's `NonReadable` and `BrokenMetadata` are both `IOError`
/// subclasses, and the callers tell them apart to decide whether to keep
/// looking or to log loudly, so the distinction is kept.
#[derive(Debug)]
pub enum Error {
    /// `openstack.NonReadable`: this is not a config drive, try the next one.
    NonReadable(String),
    /// `sources.BrokenMetadata`: it is a config drive, and it is malformed.
    Broken(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonReadable(message) | Self::Broken(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// What `read_config_drive` returns: upstream's untyped results dict.
#[derive(Debug, Default)]
pub struct Results {
    pub version: u8,
    pub metadata: Object,
    pub userdata: Option<Vec<u8>>,
    pub vendordata: Option<Value>,
    pub vendordata2: Option<Value>,
    pub networkdata: Option<Value>,
    /// The eni-formatted `network_config` a v2 drive points at, or the
    /// `etc/network/interfaces` a v1 drive carries.
    pub network_config: Option<String>,
    pub ec2_metadata: Option<Value>,
    pub dsmode: Option<String>,
    /// Files the drive asked to have injected, keyed by destination path.
    pub files: Vec<(String, Vec<u8>)>,
}

/// `BaseReader`'s abstract half: where a document lives and how to fetch it.
pub trait Source {
    /// `_path_join` then `_path_read`, relative to the base.
    fn read(&self, parts: &[&str]) -> Result<Vec<u8>, String>;

    /// `_fetch_available_versions`.
    fn versions(&self) -> Vec<String>;

    /// The location as it should appear in an error message.
    fn describe(&self, parts: &[&str]) -> String;

    /// `_read_ec2_metadata`.
    fn ec2_metadata(&self) -> Result<Option<Value>, Error>;
}

/// A config-drive directory.
struct Dir<'a>(&'a Path);

impl Source for Dir<'_> {
    fn read(&self, parts: &[&str]) -> Result<Vec<u8>, String> {
        let path = self.join(parts);
        std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))
    }

    fn versions(&self) -> Vec<String> {
        // Upstream's filter tests the parent rather than the entry (bug B38);
        // the port tests the entry, so a stray file cannot be chosen.
        let mut found: Vec<String> = std::fs::read_dir(self.0.join("openstack"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    }

    fn describe(&self, parts: &[&str]) -> String {
        self.join(parts).display().to_string()
    }

    fn ec2_metadata(&self) -> Result<Option<Value>, Error> {
        let path = self.join(&["ec2", "latest", "meta-data.json"]);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(&path).map_err(|err| {
            Error::Broken(format!("Failed to process path {}: {err}", path.display()))
        })?;
        serde_json::from_slice(&raw).map(Some).map_err(|err| {
            Error::Broken(format!("Failed to process path {}: {err}", path.display()))
        })
    }
}

impl Dir<'_> {
    fn join(&self, parts: &[&str]) -> std::path::PathBuf {
        let mut path = self.0.to_path_buf();
        for part in parts {
            path.push(part);
        }
        path
    }
}

/// `MetadataReader`: the same documents over HTTP.
struct Service<'a> {
    base: &'a str,
    config: &'a ci_url::Config,
}

impl Source for Service<'_> {
    fn read(&self, parts: &[&str]) -> Result<Vec<u8>, String> {
        let url = ci_url::url::combine_url(self.base, parts);
        match ci_url::readurl(&url, self.config) {
            Ok(response) if response.ok() => Ok(response.contents),
            Ok(response) => Err(format!("{url}: HTTP {}", response.code)),
            Err(err) => Err(err.to_string()),
        }
    }

    /// `<base>/openstack` answers with one version per line.
    fn versions(&self) -> Vec<String> {
        let Ok(raw) = self.read(&["openstack"]) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&raw)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn describe(&self, parts: &[&str]) -> String {
        ci_url::url::combine_url(self.base, parts)
    }

    /// Upstream crawls the EC2 metadata tree here; the port does not have that
    /// walk yet (deviation 73).
    fn ec2_metadata(&self) -> Result<Option<Value>, Error> {
        Ok(None)
    }
}

/// `DataSourceConfigDrive.read_config_drive`: try v2, then v1, and report the
/// last failure if neither reads.
pub fn read_config_drive(source_dir: &Path) -> Result<Results, Error> {
    match read_v2(&Dir(source_dir)) {
        Err(Error::NonReadable(_)) => read_v1(source_dir),
        other => other,
    }
}

/// `DataSourceOpenStack.read_metadata_service`.
pub fn read_metadata_service(
    base_url: &str,
    config: &ci_url::Config,
) -> Result<Results, Error> {
    read_v2(&Service {
        base: base_url,
        config,
    })
}

/// `BaseReader._find_working_version`.
fn find_working_version(source: &dyn Source) -> String {
    let available = source.versions();
    OS_VERSIONS
        .iter()
        .rev()
        .find(|version| available.iter().any(|have| have == *version))
        .map_or_else(|| OS_LATEST.to_owned(), |version| (*version).to_owned())
}

/// `BaseReader.read_v2`.
fn read_v2(source: &dyn Source) -> Result<Results, Error> {
    let version = find_working_version(source);
    let doc = |name: &str| -> Vec<String> {
        vec!["openstack".to_owned(), version.clone(), name.to_owned()]
    };
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let parts = doc(name);
        source.read(&parts.iter().map(String::as_str).collect::<Vec<_>>())
    };
    let describe = |name: &str| -> String {
        let parts = doc(name);
        source.describe(&parts.iter().map(String::as_str).collect::<Vec<_>>())
    };

    let meta_raw = read("meta_data.json")
        .map_err(|err| Error::NonReadable(format!("Missing mandatory path: {err}")))?;
    let mut metadata = load_json_object(&meta_raw, &describe("meta_data.json"))?;

    let mut results = Results {
        version: 2,
        ..Results::default()
    };
    results.userdata = read("user_data").ok();
    results.vendordata =
        parse_optional(read("vendor_data.json"), &describe("vendor_data.json"))?;
    results.vendordata2 =
        parse_optional(read("vendor_data2.json"), &describe("vendor_data2.json"))?;
    results.networkdata =
        parse_optional(read("network_data.json"), &describe("network_data.json"))?;

    // Upstream replaces the base64 text with the decoded bytes in place; the
    // port keeps it a string because the metadata map is JSON-typed.
    if let Some(seed) = metadata.get("random_seed").and_then(Value::as_str) {
        let decoded = ci_core::b64::decode(seed).ok_or_else(|| {
            Error::Broken("Badly formatted metadata random_seed entry".to_owned())
        })?;
        let seed = String::from_utf8_lossy(&decoded).into_owned();
        metadata.insert("random_seed".to_owned(), Value::String(seed));
    }

    for item in metadata
        .get("files")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        let content = read_content_path(source, item).map_err(|err| {
            Error::Broken(format!("Failed to read provided file {path}: {err}"))
        })?;
        results.files.push((path.to_owned(), content));
    }

    if let Some(item) = metadata.get("network_config") {
        let content = read_content_path(source, item).map_err(|err| {
            Error::Broken(format!("Failed to read network configuration: {err}"))
        })?;
        results.network_config = Some(String::from_utf8_lossy(&content).into_owned());
    }

    results.dsmode = metadata
        .get("meta")
        .and_then(|meta| meta.get("dsmode"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    results.ec2_metadata = source.ec2_metadata()?;

    for (target, source, required) in KEY_COPIES {
        match metadata.get(*source) {
            Some(value) => {
                let value = value.clone();
                metadata.insert((*target).to_owned(), value);
            }
            None if *required => {
                return Err(Error::Broken(format!("No '{source}' entry in metadata")));
            }
            None => {}
        }
    }

    results.metadata = metadata;
    Ok(results)
}

/// `ConfigDriveReader.read_v1`.
///
/// Every `FILES_V1` key lands in the metadata, present or not, which is why
/// `network_config` stays there rather than being promoted to [`Results`] the
/// way the v2 reader promotes it.
fn read_v1(base: &Path) -> Result<Results, Error> {
    let network = base.join("etc/network/interfaces");
    let meta_js = base.join("meta.js");
    let keys = base.join("root/.ssh/authorized_keys");
    if !network.exists() && !meta_js.exists() && !keys.exists() {
        return Err(Error::NonReadable(format!(
            "{}: no files found",
            base.display()
        )));
    }

    // Upstream reads these as bytes and leaves them that way, so they reach
    // the metadata as the `ci-b64:` strings `json_dumps` would render.
    let network_config = read_v1_bytes(&network)?;
    let authorized_keys = read_v1_bytes(&keys)?;
    let meta: Object = match read_v1_bytes(&meta_js)? {
        Some(raw) => load_json_object(&raw, &meta_js.display().to_string())?,
        None => Object::new(),
    };

    let mut metadata = Object::new();
    metadata.insert(
        "network_config".to_owned(),
        as_binary_value(network_config.as_deref()),
    );
    metadata.insert("meta_js".to_owned(), Value::Object(meta.clone()));
    metadata.insert(
        "authorized_keys".to_owned(),
        as_binary_value(authorized_keys.as_deref()),
    );

    // `meta.js` outranks the injected `authorized_keys` file. Upstream then
    // filters the injected bytes with a `str`, which raises (bug B40); the
    // port decodes first.
    let keydata = match meta.get("public-keys").and_then(Value::as_str) {
        Some(text) => Some(text.to_owned()),
        None => authorized_keys
            .as_deref()
            .map(|raw| String::from_utf8_lossy(raw).into_owned()),
    };
    if let Some(keydata) = keydata.filter(|text| !text.is_empty()) {
        let public: Vec<Value> = keydata
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| Value::String(line.to_owned()))
            .collect();
        metadata.insert("public-keys".to_owned(), Value::Array(public));
    }

    if let Some(iid) = meta.get("instance-id") {
        metadata.insert("instance-id".to_owned(), iid.clone());
    }

    Ok(Results {
        version: 1,
        userdata: meta
            .get("user-data")
            .and_then(Value::as_str)
            .map(|text| text.as_bytes().to_vec()),
        dsmode: meta
            .get("dsmode")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        metadata,
        ..Results::default()
    })
}

/// A `FILES_V1` value: the bytes upstream read, or its `""` default.
fn as_binary_value(raw: Option<&[u8]>) -> Value {
    Value::String(raw.map_or_else(String::new, |bytes| {
        format!("ci-b64:{}", ci_core::b64::encode(bytes))
    }))
}

/// A v1 file that may be absent, read as the bytes upstream reads.
fn read_v1_bytes(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::read(path).map(Some).map_err(|err| {
        Error::Broken(format!("Failed to read: {}: {err}", path.display()))
    })
}

/// `BaseReader._read_content_path`.
fn read_content_path(source: &dyn Source, item: &Value) -> Result<Vec<u8>, Error> {
    let raw = item
        .get("content_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pieces: Vec<&str> = raw
        .trim_start_matches('/')
        .split('/')
        .filter(|piece| !piece.is_empty())
        .collect();
    if pieces.is_empty() {
        return Err(Error::Broken(format!(
            "Item {item} has no valid content path"
        )));
    }
    let mut parts = vec!["openstack"];
    parts.extend(pieces);
    source.read(&parts).map_err(Error::Broken)
}

/// An optional v2 document, parsed as `load_json` with `root_types` widened to
/// anything, which is what upstream passes for the vendor and network files.
fn parse_optional(
    raw: Result<Vec<u8>, String>,
    where_: &str,
) -> Result<Option<Value>, Error> {
    let Ok(raw) = raw else {
        return Ok(None);
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|err| Error::Broken(format!("Failed to process path {where_}: {err}")))
}

/// `util.load_json`, which insists on a mapping at the root.
fn load_json_object(raw: &[u8], where_: &str) -> Result<Object, Error> {
    let value: Value = serde_json::from_slice(raw).map_err(|err| {
        Error::Broken(format!("Failed to process path {where_}: {err}"))
    })?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(Error::Broken(format!(
            "Failed to process path {where_}: expected dict, got {}",
            type_name(&other)
        ))),
    }
}

/// The Python type name `load_json`'s error would carry.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) => {
            if number.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
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

    fn write(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn a_v2_drive_reads_its_metadata_and_applies_the_key_renames() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc", "hostname": "host1"}"#,
        );
        write(dir.path(), "openstack/2018-08-27/user_data", "#!/bin/sh\n");

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(results.version, 2);
        assert_eq!(results.metadata["instance-id"], "i-abc");
        assert_eq!(results.metadata["local-hostname"], "host1");
        assert_eq!(results.userdata.unwrap(), b"#!/bin/sh\n");
    }

    #[test]
    fn a_v2_drive_without_a_uuid_is_broken_rather_than_absent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "openstack/2018-08-27/meta_data.json", "{}");

        let error = read_config_drive(dir.path()).unwrap_err();

        assert!(matches!(error, Error::Broken(_)), "{error:?}");
        assert_eq!(error.to_string(), "No 'uuid' entry in metadata");
    }

    #[test]
    fn a_stray_file_named_after_a_version_does_not_hide_the_real_one() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "openstack/2013-04-04/meta_data.json",
            r#"{"uuid": "i-abc"}"#,
        );
        // Upstream selects this file as if it were a version directory and
        // then reports the whole drive unreadable (bug B38).
        write(dir.path(), "openstack/2018-08-27", "not a directory\n");

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(results.metadata["instance-id"], "i-abc");
    }

    #[test]
    fn the_newest_version_directory_present_wins() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["2012-08-10", "2016-10-06", "2013-04-04"] {
            write(
                dir.path(),
                &format!("openstack/{version}/meta_data.json"),
                &format!(r#"{{"uuid": "{version}"}}"#),
            );
        }

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(results.metadata["instance-id"], "2016-10-06");
    }

    #[test]
    fn an_injected_file_is_read_through_its_content_path() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "openstack/2018-08-27/meta_data.json",
            r#"{"uuid": "i-abc",
                "files": [{"path": "/etc/motd", "content_path": "/content/0000"}]}"#,
        );
        write(dir.path(), "openstack/content/0000", "hello\n");

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(
            results.files,
            vec![("/etc/motd".to_owned(), b"hello\n".to_vec())]
        );
    }

    #[test]
    fn a_v1_drive_falls_back_when_there_is_no_openstack_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "meta.js",
            r#"{"instance-id": "i-v1", "user-data": "hi"}"#,
        );
        write(
            dir.path(),
            "root/.ssh/authorized_keys",
            "ssh-rsa AAAA\n# comment\n",
        );

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(results.version, 1);
        assert_eq!(results.metadata["instance-id"], "i-v1");
        assert_eq!(
            results.metadata["public-keys"],
            serde_json::json!(["ssh-rsa AAAA"])
        );
        assert_eq!(results.userdata.unwrap(), b"hi");
    }

    #[test]
    fn an_injected_authorized_keys_file_is_filtered_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "meta.js", r#"{"instance-id": "i-v1"}"#);
        // Upstream reads this as bytes and then filters it with a `str`,
        // which raises `TypeError` (bug B40).
        write(
            dir.path(),
            "root/.ssh/authorized_keys",
            "ssh-rsa AAAA one\n# comment\n\nssh-rsa BBBB two\n",
        );

        let results = read_config_drive(dir.path()).unwrap();

        assert_eq!(
            results.metadata["public-keys"],
            serde_json::json!(["ssh-rsa AAAA one", "ssh-rsa BBBB two"])
        );
    }

    #[test]
    fn an_empty_directory_is_not_a_config_drive() {
        let dir = tempfile::tempdir().unwrap();

        let error = read_config_drive(dir.path()).unwrap_err();

        assert!(matches!(error, Error::NonReadable(_)), "{error:?}");
    }
}
