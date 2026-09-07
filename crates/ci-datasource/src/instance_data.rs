//! `DataSource.persist_instance_data`: `instance-data.json`, its sensitive
//! twin, and the `cloud-id` run files.

use std::io;
use std::path::Path;

use ci_config::{Object, Value};
use ci_core::Lookup;
use ci_sys::atomic::{self, WriteOptions};

use crate::types::{Context, Datasource};

/// `EXPERIMENTAL_TEXT`.
const EXPERIMENTAL_TEXT: &str =
    "EXPERIMENTAL: The structure and format of content scoped under \
     the 'ds' key may change in subsequent releases of cloud-init.";

/// `REDACT_SENSITIVE_VALUE`.
const REDACT_SENSITIVE_VALUE: &str = "redacted for non-root user";

/// `DataSource.sensitive_metadata_keys`. Matched against a key name or a whole
/// `a/b/c` path, case-insensitively.
const SENSITIVE_KEYS: [&str; 9] = [
    "combined_cloud_config",
    "merged_cfg",
    "merged_system_cfg",
    "security-credentials",
    "userdata",
    "user-data",
    "user_data",
    "vendordata",
    "vendor-data",
];

/// Writes `instance-data.json`, `instance-data-sensitive.json` and the
/// `cloud-id` files.
///
/// Upstream also pickles the datasource here; the port has no `obj.pkl`
/// (COMPAT.md deviation 1).
pub fn persist(ds: &Datasource, ctx: &mut Context<'_>) -> io::Result<()> {
    let instance_data = build(ds, ctx.sys_cfg);
    let processed = process(&Value::Object(instance_data), "");

    let cloud_id = processed
        .get("v1")
        .and_then(|v1| v1.get("cloud_id"))
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_owned();
    write_cloud_id(&ctx.paths.run_dir, &cloud_id)?;

    write_json(
        &ctx.paths.run_path(Lookup::InstanceDataSensitive),
        &Value::Object(processed.clone()),
        0o600,
    )?;
    write_json(
        &ctx.paths.run_path(Lookup::InstanceData),
        &Value::Object(redact(&processed)),
        0o644,
    )
}

/// The document before `process_instance_metadata` touches it.
fn build(ds: &Datasource, sys_cfg: &Object) -> Object {
    let sys_info = ci_core::sysinfo::system_info();

    let mut meta = Object::new();
    meta.insert("meta_data".to_owned(), Value::Object(ds.metadata.clone()));
    meta.insert(
        "_doc".to_owned(),
        Value::String(EXPERIMENTAL_TEXT.to_owned()),
    );

    let mut merged_cfg = sys_cfg.clone();
    merged_cfg.insert(
        "_doc".to_owned(),
        Value::String(
            "DEPRECATED: Use merged_system_cfg. Will be dropped from 24.1".to_owned(),
        ),
    );
    let mut merged_system_cfg = sys_cfg.clone();
    merged_system_cfg.insert(
        "_doc".to_owned(),
        Value::String(
            "Merged cloud-init system config from /etc/cloud/cloud.cfg and \
             /etc/cloud/cloud.cfg.d/"
                .to_owned(),
        ),
    );

    let mut out = Object::new();
    out.insert("ds".to_owned(), Value::Object(meta));
    out.insert("merged_cfg".to_owned(), Value::Object(merged_cfg));
    out.insert(
        "merged_system_cfg".to_owned(),
        Value::Object(merged_system_cfg),
    );
    out.insert("v1".to_owned(), Value::Object(standardized(ds, &sys_info)));
    out.insert("sys_info".to_owned(), Value::Object(sys_info));
    out
}

/// `_get_standardized_metadata`.
fn standardized(ds: &Datasource, sys_info: &Object) -> Object {
    let sys_str = |key: &str| {
        sys_info
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let sys_at = |key: &str, index: usize| {
        sys_info
            .get(key)
            .and_then(Value::as_array)
            .and_then(|list| list.get(index))
            .cloned()
            .unwrap_or(Value::String(String::new()))
    };

    let hostname = Value::String(hostname(ds, sys_info));
    let instance_id = Value::String(ds.instance_id.clone());
    let zone = ds.availability_zone().cloned().unwrap_or(Value::Null);
    let cloud_name = Value::String(ds.cloud_name());

    let mut v1 = Object::new();
    v1.insert(
        "_beta_keys".to_owned(),
        Value::Array(vec![Value::String("subplatform".to_owned())]),
    );
    v1.insert("availability-zone".to_owned(), zone.clone());
    v1.insert("availability_zone".to_owned(), zone);
    v1.insert("cloud_id".to_owned(), Value::String(ds.cloud_id()));
    v1.insert("cloud-name".to_owned(), cloud_name.clone());
    v1.insert("cloud_name".to_owned(), cloud_name);
    v1.insert("distro".to_owned(), sys_at("dist", 0));
    v1.insert("distro_version".to_owned(), sys_at("dist", 1));
    v1.insert("distro_release".to_owned(), sys_at("dist", 2));
    v1.insert(
        "platform".to_owned(),
        Value::String(ds.platform_type.clone()),
    );
    v1.insert(
        "public_ssh_keys".to_owned(),
        Value::Array(public_ssh_keys(ds)),
    );
    v1.insert(
        "python_version".to_owned(),
        Value::String(sys_str("python")),
    );
    v1.insert("instance-id".to_owned(), instance_id.clone());
    v1.insert("instance_id".to_owned(), instance_id);
    v1.insert("kernel_release".to_owned(), sys_at("uname", 2));
    v1.insert("local-hostname".to_owned(), hostname.clone());
    v1.insert("local_hostname".to_owned(), hostname);
    v1.insert("machine".to_owned(), sys_at("uname", 4));
    v1.insert(
        "region".to_owned(),
        ds.region().cloned().unwrap_or(Value::Null),
    );
    v1.insert(
        "subplatform".to_owned(),
        Value::String(ds.subplatform.clone()),
    );
    v1.insert(
        "system_platform".to_owned(),
        Value::String(sys_str("platform")),
    );
    v1.insert("variant".to_owned(), Value::String(sys_str("variant")));
    v1
}

/// `get_hostname().hostname`, without the `resolve_ip` and `/etc/hosts` lookups
/// that only the fully-qualified forms need.
fn hostname(ds: &Datasource, sys_info: &Object) -> String {
    let local = ds
        .metadata
        .get("local-hostname")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if local.is_empty() {
        let node = sys_info
            .get("uname")
            .and_then(Value::as_array)
            .and_then(|list| list.get(1))
            .and_then(Value::as_str)
            .unwrap_or("localhost");
        return node.split('.').next().unwrap_or(node).to_owned();
    }
    if is_ipv4(local) {
        return format!("ip-{}", local.replace('.', "-"));
    }
    local.split('.').next().unwrap_or(local).to_owned()
}

fn is_ipv4(value: &str) -> bool {
    value.parse::<std::net::Ipv4Addr>().is_ok()
}

/// `normalize_pubkey_data`.
fn public_ssh_keys(ds: &Datasource) -> Vec<Value> {
    match ds.metadata.get("public-keys") {
        Some(Value::String(text)) => {
            text.lines().map(|l| Value::String(l.to_owned())).collect()
        }
        Some(Value::Array(list)) => list.clone(),
        Some(Value::Object(map)) => map
            .values()
            .flat_map(|entry| match entry {
                Value::String(key) => vec![Value::String(key.clone())],
                Value::Array(keys) => keys.clone(),
                _ => Vec::new(),
            })
            .filter(|key| key.as_str() != Some(""))
            .collect(),
        _ => Vec::new(),
    }
}

/// `process_instance_metadata`: strip the `ci-b64:` prefix, and catalogue the
/// base64-encoded and sensitive key paths.
fn process(value: &Value, key_path: &str) -> Object {
    let Some(map) = value.as_object() else {
        return Object::new();
    };
    let mut out = map.clone();
    let mut base64 = Vec::new();
    let mut sensitive = Vec::new();

    for (key, val) in map {
        let sub_path = if key_path.is_empty() {
            key.clone()
        } else {
            format!("{key_path}/{key}")
        };
        let lower_key = key.to_lowercase();
        let lower_path = sub_path.to_lowercase();
        if SENSITIVE_KEYS.contains(&lower_key.as_str())
            || SENSITIVE_KEYS.contains(&lower_path.as_str())
        {
            sensitive.push(sub_path.clone());
        }
        if let Some(text) = val.as_str().and_then(|t| t.strip_prefix("ci-b64:")) {
            base64.push(sub_path.clone());
            out.insert(key.clone(), Value::String(text.to_owned()));
        }
        if val.is_object() {
            let mut nested = process(val, &sub_path);
            extend(&mut base64, nested.remove("base64_encoded_keys"));
            extend(&mut sensitive, nested.remove("sensitive_keys"));
            out.insert(key.clone(), Value::Object(nested));
        }
    }

    base64.sort();
    sensitive.sort();
    out.insert("base64_encoded_keys".to_owned(), strings(base64));
    out.insert("sensitive_keys".to_owned(), strings(sensitive));
    out
}

fn extend(into: &mut Vec<String>, from: Option<Value>) {
    if let Some(Value::Array(list)) = from {
        into.extend(
            list.into_iter()
                .filter_map(|entry| entry.as_str().map(ToOwned::to_owned)),
        );
    }
}

fn strings(list: Vec<String>) -> Value {
    Value::Array(list.into_iter().map(Value::String).collect())
}

/// `redact_sensitive_keys`.
fn redact(metadata: &Object) -> Object {
    let mut out = metadata.clone();
    let Some(Value::Array(paths)) = metadata.get("sensitive_keys") else {
        return out;
    };
    for path in paths {
        let Some(path) = path.as_str() else { continue };
        let parts: Vec<&str> = path.split('/').collect();
        if let Some((last, parents)) = parts.split_last() {
            redact_path(&mut out, parents, last);
        }
    }
    out
}

/// A parent that is missing, or is not a mapping, does not stop the walk:
/// upstream keeps testing the remaining parts against the object it is already
/// holding.
fn redact_path(obj: &mut Object, parents: &[&str], last: &str) {
    if let Some((head, rest)) = parents.split_first() {
        if let Some(Value::Object(inner)) = obj.get_mut(*head) {
            redact_path(inner, rest, last);
        } else {
            redact_path(obj, rest, last);
        }
    } else if obj.contains_key(last) {
        obj.insert(
            last.to_owned(),
            Value::String(REDACT_SENSITIVE_VALUE.to_owned()),
        );
    }
}

/// `cloud-id-<id>` plus the `cloud-id` symlink, dropping the file the previous
/// boot's symlink pointed at.
fn write_cloud_id(run_dir: &Path, cloud_id: &str) -> io::Result<()> {
    let link = run_dir.join("cloud-id");
    let target = run_dir.join(format!("cloud-id-{cloud_id}"));
    let previous = std::fs::canonicalize(&link).ok();

    ci_sys::path::ensure_dir(run_dir, 0o755)?;
    atomic::write_file(
        &target,
        format!("{cloud_id}\n").as_bytes(),
        WriteOptions::PUBLIC,
    )?;
    ci_sys::path::sym_link(&target, &link, true)?;
    if let Some(previous) = previous {
        if previous != target {
            let _ = std::fs::remove_file(previous);
        }
    }
    Ok(())
}

fn write_json(path: &Path, value: &Value, mode: u32) -> io::Result<()> {
    let body = format!("{}\n", ci_core::dumps_indent(value, 1));
    atomic::write_file(path, body.as_bytes(), WriteOptions::mode(mode))
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
    use crate::types::DsMode;

    fn sample() -> Datasource {
        let mut metadata = Object::new();
        metadata.insert(
            "instance-id".to_owned(),
            Value::String("iid-local01".to_owned()),
        );
        metadata.insert("local-hostname".to_owned(), Value::String("me".to_owned()));
        Datasource {
            class_name: "DataSourceNoCloud",
            dsname: "NoCloud",
            dsmode: DsMode::Network,
            instance_id: "iid-local01".to_owned(),
            metadata,
            userdata_raw: None,
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
    fn the_merged_config_is_catalogued_as_sensitive_under_both_names() {
        let mut sys_cfg = Object::new();
        sys_cfg.insert("locale".to_owned(), Value::String("fr_FR".to_owned()));
        let processed = process(&Value::Object(build(&sample(), &sys_cfg)), "");
        assert_eq!(
            processed["sensitive_keys"],
            serde_json::json!(["merged_cfg", "merged_system_cfg"])
        );
        assert_eq!(processed["base64_encoded_keys"], serde_json::json!([]));
    }

    #[test]
    fn redaction_replaces_the_whole_subtree_not_just_its_leaves() {
        let mut sys_cfg = Object::new();
        sys_cfg.insert("locale".to_owned(), Value::String("fr_FR".to_owned()));
        let processed = process(&Value::Object(build(&sample(), &sys_cfg)), "");
        let public = redact(&processed);
        assert_eq!(
            public["merged_cfg"],
            Value::String(REDACT_SENSITIVE_VALUE.to_owned())
        );
        assert_eq!(
            public["merged_system_cfg"],
            Value::String(REDACT_SENSITIVE_VALUE.to_owned())
        );
        // The sensitive copy keeps them.
        assert!(processed["merged_cfg"].is_object());
    }

    #[test]
    fn a_ci_b64_prefix_is_stripped_and_the_key_path_recorded() {
        let mut inner = Object::new();
        inner.insert("blob".to_owned(), Value::String("ci-b64:aGk=".to_owned()));
        let mut root = Object::new();
        root.insert("ds".to_owned(), Value::Object(inner));
        let processed = process(&Value::Object(root), "");
        assert_eq!(processed["ds"]["blob"], Value::String("aGk=".to_owned()));
        assert_eq!(
            processed["base64_encoded_keys"],
            serde_json::json!(["ds/blob"])
        );
    }

    #[test]
    fn the_local_hostname_loses_its_domain() {
        let mut ds = sample();
        ds.metadata.insert(
            "local-hostname".to_owned(),
            Value::String("host.example.com".to_owned()),
        );
        assert_eq!(hostname(&ds, &Object::new()), "host");
    }

    #[test]
    fn an_ipv4_local_hostname_becomes_an_ip_prefixed_name() {
        let mut ds = sample();
        ds.metadata.insert(
            "local-hostname".to_owned(),
            Value::String("10.0.0.5".to_owned()),
        );
        assert_eq!(hostname(&ds, &Object::new()), "ip-10-0-0-5");
    }

    #[test]
    fn public_keys_are_flattened_out_of_whatever_shape_they_arrive_in() {
        let mut ds = sample();
        ds.metadata.insert(
            "public-keys".to_owned(),
            Value::String("ssh-rsa AAA\nssh-rsa BBB".to_owned()),
        );
        assert_eq!(
            public_ssh_keys(&ds),
            vec![
                Value::String("ssh-rsa AAA".to_owned()),
                Value::String("ssh-rsa BBB".to_owned())
            ]
        );

        ds.metadata.insert(
            "public-keys".to_owned(),
            serde_json::json!({"0": ["ssh-rsa AAA", ""], "1": "ssh-rsa BBB"}),
        );
        assert_eq!(
            public_ssh_keys(&ds),
            vec![
                Value::String("ssh-rsa AAA".to_owned()),
                Value::String("ssh-rsa BBB".to_owned())
            ]
        );
    }

    #[test]
    fn the_cloud_id_symlink_replaces_the_file_the_last_boot_left() {
        let dir = tempfile::tempdir().unwrap();
        write_cloud_id(dir.path(), "nocloud").unwrap();
        assert!(dir.path().join("cloud-id-nocloud").exists());

        write_cloud_id(dir.path(), "lxd").unwrap();
        assert!(dir.path().join("cloud-id-lxd").exists());
        assert!(!dir.path().join("cloud-id-nocloud").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cloud-id")).unwrap(),
            "lxd\n"
        );
    }
}
