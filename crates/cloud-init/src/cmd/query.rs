//! `cloud-init query` — port of `cloudinit/cmd/query.py`.

use std::path::{Path, PathBuf};

use ci_config::Value;
use ci_core::Paths;
use clap::Args as ClapArgs;

const REDACT_SENSITIVE_VALUE: &str = "redacted for non-root user";

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Add verbose messages.
    #[arg(long, short = 'd')]
    pub debug: bool,

    /// Path to instance-data.json file.
    #[arg(long, short = 'i')]
    pub instance_data: Option<PathBuf>,

    /// List query keys available at the provided instance-data <varname>.
    #[arg(long, short = 'l')]
    pub list_keys: bool,

    /// Path to user-data file.
    #[arg(long, short = 'u')]
    pub user_data: Option<PathBuf>,

    /// Path to vendor-data file.
    #[arg(long, short = 'v')]
    pub vendor_data: Option<PathBuf>,

    /// Dump all available instance-data.
    #[arg(long, short = 'a', conflicts_with = "varname")]
    pub all: bool,

    /// Optionally specify a custom output format string.
    #[arg(long, short = 'f')]
    pub format: Option<String>,

    /// A dot-delimited instance data variable to query from instance-data.
    pub varname: Option<String>,
}

pub fn run(args: &Args) -> u8 {
    if !args.list_keys && args.varname.is_none() && !args.all && args.format.is_none() {
        eprintln!(
            "Expected one of the options: --all, --format, --list-keys or varname"
        );
        return 1;
    }

    let instance_data = match load_instance_data(args) {
        Ok(data) => data,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };

    if let Some(format) = &args.format {
        let payload = format!("## template: jinja\n{format}");
        let params =
            ci_template::convert_jinja_instance_data_with_aliases(&instance_data);
        return match ci_template::render_string(&payload, &params) {
            Ok(rendered) if !rendered.is_empty() => {
                println!("{rendered}");
                0
            }
            _ => 1,
        };
    }

    // Both views are needed: the alias-augmented tree drives path resolution, but
    // the response must come from the unaliased tree so output stays canonical.
    let response_root = ci_template::convert_jinja_instance_data(&instance_data);
    let aliased_root =
        ci_template::convert_jinja_instance_data_with_aliases(&instance_data);

    let response =
        match find_leaf(&response_root, &aliased_root, args.varname.as_deref()) {
            Ok(value) => value,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        };

    let text = if args.list_keys {
        let Some(map) = response.as_object() else {
            eprintln!(
                "--list-keys provided but '{}' is not a dict",
                args.varname.as_deref().unwrap_or("None")
            );
            return 1;
        };
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys.join("\n")
    } else if let Value::String(s) = response {
        s.clone()
    } else {
        ci_core::json_dumps(response)
    };

    println!("{text}");
    0
}

/// Load `instance-data.json`, splicing in user-data and vendor-data.
///
/// Non-root callers never see raw user-data or the sensitive instance-data file;
/// they get an explicit redaction marker instead, matching upstream.
fn load_instance_data(args: &Args) -> Result<Value, String> {
    let paths = Paths::read();
    let instance_data_fn =
        super::instance_data_path(&paths, args.instance_data.as_deref());

    let user_data_fn = args
        .user_data
        .clone()
        .unwrap_or_else(|| paths.instance_link().join("user-data.txt"));
    let vendor_data_fn = args
        .vendor_data
        .clone()
        .unwrap_or_else(|| paths.instance_link().join("vendor-data.txt"));
    let combined_fn = paths.run_path(ci_core::paths::Lookup::CombinedCloudConfig);

    let raw = ci_sys::path::read_text_capped(
        &instance_data_fn,
        ci_sys::path::DEFAULT_MAX_BYTES,
    )
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "No read permission on '{}'. Try sudo",
            instance_data_fn.display()
        ),
        _ => format!("Missing instance-data file: {}", instance_data_fn.display()),
    })?;

    let mut data: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{}: {e}", instance_data_fn.display()))?;

    let Some(map) = data.as_object_mut() else {
        return Err(format!(
            "{}: expected a JSON object",
            instance_data_fn.display()
        ));
    };

    if ci_sys::is_root() {
        map.insert(
            "userdata".into(),
            Value::String(read_payload(&user_data_fn)),
        );
        map.insert(
            "vendordata".into(),
            Value::String(read_payload(&vendor_data_fn)),
        );
        // Absent until the init stage merges vendor-data and user-data.
        let combined = ci_sys::path::read_text_optional(
            &combined_fn,
            ci_sys::path::DEFAULT_MAX_BYTES,
        )
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
        map.insert("combined_cloud_config".into(), combined);
    } else {
        map.insert("userdata".into(), Value::String(redacted(&user_data_fn)));
        map.insert(
            "vendordata".into(),
            Value::String(redacted(&vendor_data_fn)),
        );
        map.insert(
            "combined_cloud_config".into(),
            Value::String(redacted(&combined_fn)),
        );
    }

    Ok(data)
}

fn redacted(path: &Path) -> String {
    format!("<{REDACT_SENSITIVE_VALUE}> file:{}", path.display())
}

fn read_payload(path: &Path) -> String {
    ci_sys::path::read_text_optional(path, ci_sys::path::DEFAULT_MAX_BYTES)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Walk a dot-delimited path, as `_find_instance_data_leaf_by_varname_path` does.
///
/// `aliased` may contain underscore aliases for keys that hold jinja operators;
/// it decides whether the path exists, while the returned value always comes from
/// the canonical `root`.
fn find_leaf<'a>(
    root: &'a Value,
    aliased: &Value,
    varname: Option<&str>,
) -> Result<&'a Value, String> {
    let Some(varname) = varname else {
        return Ok(root);
    };

    let mut walked = String::new();
    let mut current = root;
    let mut current_aliased = aliased;
    for part in varname.split('.') {
        current_aliased = current_aliased.get(part).ok_or_else(|| {
            if walked.is_empty() {
                format!("Undefined instance-data key '{varname}'")
            } else {
                format!("instance-data '{walked}' has no '{part}'")
            }
        })?;

        current = match current.get(part) {
            Some(value) => value,
            // The path component was an underscore alias; find the real key.
            None => current
                .as_object()
                .and_then(|map| {
                    map.iter().find_map(|(key, value)| {
                        (ci_template::jinja_variable_alias(key).as_deref()
                            == Some(part))
                        .then_some(value)
                    })
                })
                .ok_or_else(|| format!("instance-data '{walked}' has no '{part}'"))?,
        };

        if !walked.is_empty() {
            walked.push('.');
        }
        walked.push_str(part);
    }
    Ok(current)
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
    use serde_json::json;

    fn roots(value: &Value) -> (Value, Value) {
        (
            ci_template::convert_jinja_instance_data(value),
            ci_template::convert_jinja_instance_data_with_aliases(value),
        )
    }

    #[test]
    fn walks_dotted_paths() {
        let (root, aliased) = roots(&json!({"v1": {"cloud_name": "aws"}}));
        assert_eq!(
            find_leaf(&root, &aliased, Some("v1.cloud_name")).unwrap(),
            "aws"
        );
    }

    #[test]
    fn version_namespaces_are_hoisted_to_the_top_level() {
        let (root, aliased) = roots(&json!({"v1": {"cloud_name": "aws"}}));
        assert_eq!(
            find_leaf(&root, &aliased, Some("cloud_name")).unwrap(),
            "aws"
        );
    }

    #[test]
    fn underscore_aliases_resolve_to_the_canonical_key() {
        let (root, aliased) = roots(&json!({"ds": {"meta-data": {"id": "i-1"}}}));
        assert_eq!(
            find_leaf(&root, &aliased, Some("ds.meta_data.id")).unwrap(),
            "i-1"
        );
        // The canonical tree keeps the original spelling.
        assert!(root["ds"].get("meta_data").is_none());
    }

    #[test]
    fn missing_keys_use_the_upstream_messages() {
        let (root, aliased) = roots(&json!({"v1": {"cloud_name": "aws"}}));
        assert_eq!(
            find_leaf(&root, &aliased, Some("v1.nope")).unwrap_err(),
            "instance-data 'v1' has no 'nope'"
        );
        assert_eq!(
            find_leaf(&root, &aliased, Some("nope")).unwrap_err(),
            "Undefined instance-data key 'nope'"
        );
    }

    #[test]
    fn descending_into_a_scalar_is_an_error() {
        let (root, aliased) = roots(&json!({"v1": {"cloud_name": "aws"}}));
        assert_eq!(
            find_leaf(&root, &aliased, Some("v1.cloud_name.x")).unwrap_err(),
            "instance-data 'v1.cloud_name' has no 'x'"
        );
    }

    #[test]
    fn no_varname_returns_the_root() {
        let (root, aliased) = roots(&json!({"v1": {}}));
        assert_eq!(find_leaf(&root, &aliased, None).unwrap(), &root);
    }
}
