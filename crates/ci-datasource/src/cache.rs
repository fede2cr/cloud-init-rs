//! `sources.pkl_store` / `sources.pkl_load`.
//!
//! Upstream pickles the live `DataSource` object. A pickle is a Python object
//! graph — it names classes, and reconstructing one would mean claiming to be
//! `cloudinit.sources.DataSourceNoCloud` with the exact attribute set of the
//! installed version. The port writes JSON at the same path instead
//! (COMPAT.md deviation 67); both implementations reject the other's file and
//! fall back to a fresh search, which is what upstream already does for a
//! cache it cannot read.

use std::io;
use std::path::Path;

use ci_config::{Object, Value};
use ci_core::b64;
use ci_sys::atomic::{self, WriteOptions};

use crate::types::{Datasource, DsMode};

/// Bumped whenever the shape below changes; an older or newer cache is ignored
/// rather than misread. Upstream's equivalent is `_ci_pkl_version`.
const CACHE_VERSION: u64 = 1;

/// `pkl_store`. Errors are the caller's to log; upstream logs and returns false.
pub fn store(ds: &Datasource, path: &Path) -> io::Result<()> {
    let mut out = Object::new();
    out.insert("_cache_version".to_owned(), Value::from(CACHE_VERSION));
    out.insert(
        "class_name".to_owned(),
        Value::String(ds.class_name.to_owned()),
    );
    out.insert("dsmode".to_owned(), Value::String(ds.dsmode.to_string()));
    out.insert(
        "instance_id".to_owned(),
        Value::String(ds.instance_id.clone()),
    );
    out.insert("metadata".to_owned(), Value::Object(ds.metadata.clone()));
    out.insert("userdata_raw".to_owned(), raw(ds.userdata_raw.as_deref()));
    out.insert(
        "vendordata_raw".to_owned(),
        raw(ds.vendordata_raw.as_deref()),
    );
    out.insert(
        "vendordata2_raw".to_owned(),
        raw(ds.vendordata2_raw.as_deref()),
    );
    out.insert(
        "network_config".to_owned(),
        ds.network_config.clone().unwrap_or(Value::Null),
    );
    out.insert(
        "platform_type".to_owned(),
        Value::String(ds.platform_type.clone()),
    );
    out.insert(
        "subplatform".to_owned(),
        Value::String(ds.subplatform.clone()),
    );
    out.insert(
        "cloud_name_default".to_owned(),
        Value::String(ds.cloud_name_default.clone()),
    );
    out.insert("detail".to_owned(), Value::String(ds.detail.clone()));

    if let Some(parent) = path.parent() {
        ci_sys::path::ensure_dir(parent, 0o755)?;
    }
    let body = format!("{}\n", ci_core::dumps_indent(&Value::Object(out), 1));
    atomic::write_file(path, body.as_bytes(), WriteOptions::mode(0o400))
}

/// `pkl_load`: a missing, unreadable or unrecognised cache is not an error.
///
/// A cache naming a datasource this build does not have is rejected too, since
/// there would be no `check_instance_id` to validate it with.
#[must_use]
pub fn load(path: &Path) -> Option<Datasource> {
    let text = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let map = parsed.as_object()?;
    if map.get("_cache_version").and_then(Value::as_u64)? != CACHE_VERSION {
        return None;
    }
    let class_name = map.get("class_name").and_then(Value::as_str)?;
    let probe = crate::search::probe_for_class(class_name)?;

    Some(Datasource {
        class_name: probe.class_name(),
        dsname: probe.dsname(),
        dsmode: DsMode::parse(map.get("dsmode").and_then(Value::as_str)?)?,
        instance_id: string(map, "instance_id")?,
        metadata: map.get("metadata")?.as_object()?.clone(),
        userdata_raw: bytes(map, "userdata_raw")?,
        vendordata_raw: bytes(map, "vendordata_raw")?,
        vendordata2_raw: bytes(map, "vendordata2_raw")?,
        network_config: match map.get("network_config")? {
            Value::Null => None,
            other => Some(other.clone()),
        },
        platform_type: string(map, "platform_type")?,
        subplatform: string(map, "subplatform")?,
        cloud_name_default: string(map, "cloud_name_default")?,
        detail: string(map, "detail")?,
    })
}

/// User data may be a gzip blob or anything else, so it does not go in as text.
fn raw(data: Option<&[u8]>) -> Value {
    data.map_or(Value::Null, |bytes| Value::String(b64::encode(bytes)))
}

/// `Ok(None)` and "absent" are different answers; a key that is present but
/// malformed rejects the whole cache rather than silently losing user data.
#[allow(clippy::option_option)]
fn bytes(map: &Object, key: &str) -> Option<Option<Vec<u8>>> {
    match map.get(key)? {
        Value::Null => Some(None),
        Value::String(text) => b64::decode(text).map(Some),
        _ => None,
    }
}

fn string(map: &Object, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
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

    fn sample() -> Datasource {
        let mut metadata = Object::new();
        metadata.insert(
            "instance-id".to_owned(),
            Value::String("iid-local01".to_owned()),
        );
        Datasource {
            class_name: "DataSourceNoCloud",
            dsname: "NoCloud",
            dsmode: DsMode::Network,
            instance_id: "iid-local01".to_owned(),
            metadata,
            userdata_raw: Some(b"#cloud-config\n".to_vec()),
            vendordata_raw: None,
            vendordata2_raw: None,
            network_config: None,
            platform_type: "nocloud".to_owned(),
            subplatform: "seed-dir (/seed)".to_owned(),
            cloud_name_default: "unknown".to_owned(),
            detail: " [seed=/seed]".to_owned(),
        }
    }

    #[test]
    fn a_stored_datasource_comes_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        let ds = sample();
        store(&ds, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.class_name, ds.class_name);
        assert_eq!(back.dsname, ds.dsname);
        assert_eq!(back.dsmode, ds.dsmode);
        assert_eq!(back.instance_id, ds.instance_id);
        assert_eq!(back.metadata, ds.metadata);
        assert_eq!(back.userdata_raw, ds.userdata_raw);
        assert_eq!(back.subplatform, ds.subplatform);
        assert_eq!(back.detail, ds.detail);
        assert_eq!(back.to_string(), ds.to_string());
    }

    #[test]
    fn the_cache_is_only_readable_by_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        store(&sample(), &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o400);
    }

    #[test]
    fn user_data_that_is_not_text_survives_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        let mut ds = sample();
        ds.userdata_raw = Some(vec![0x1f, 0x8b, 0x08, 0x00, 0xff, 0xfe]);
        store(&ds, &path).unwrap();
        assert_eq!(load(&path).unwrap().userdata_raw, ds.userdata_raw);
    }

    #[test]
    fn a_pickle_from_the_python_implementation_is_not_a_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        std::fs::write(&path, b"\x80\x05\x95\x00\x00\x00\x00").unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn a_cache_from_another_version_of_the_format_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        store(&sample(), &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("\"_cache_version\": 1", "\"_cache_version\": 2"),
        )
        .unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn a_cache_naming_an_unknown_datasource_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obj.pkl");
        store(&sample(), &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // Any class the registry does not build; OpenNebula is in upstream's
        // `datasource_list` and is not ported.
        std::fs::write(
            &path,
            text.replace("DataSourceNoCloud", "DataSourceOpenNebula"),
        )
        .unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        assert!(load(Path::new("/nonexistent/obj.pkl")).is_none());
    }
}
