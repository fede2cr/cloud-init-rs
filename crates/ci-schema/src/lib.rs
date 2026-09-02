//! JSON Schema validation with cloud-init's error message contract.
//!
//! `cloud-init schema` prints the messages the Python `jsonschema` library
//! produces, so this crate reimplements draft-04 validation *and* those exact
//! strings rather than using an off-the-shelf validator. See docs/COMPAT.md.

#![forbid(unsafe_code)]

use std::sync::OnceLock;

pub mod marks;
pub mod repr;
pub mod validate;

pub use repr::repr;
pub use validate::{Error, Kind, Schema, Seg};

/// `schemas/schema-cloud-config-v1.json`, vendored from upstream cloud-init.
///
/// cloud-init is licensed `Apache-2.0 OR GPL-3.0-only`, the same terms as this
/// project, so the document ships unmodified. It is the authoritative
/// description of cloud-config and cannot be reconstructed independently
/// without diverging from what operators validate against today.
const CLOUD_CONFIG_SCHEMA: &str =
    include_str!("../schemas/schema-cloud-config-v1.json");

/// The parsed cloud-config schema, loaded once.
///
/// # Panics
///
/// If the vendored document is not valid JSON, which a build could not produce.
pub fn cloud_config_schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        // The input is a compile-time constant checked by the crate's tests, so
        // a failure here is a build defect rather than a runtime condition.
        #[allow(clippy::expect_used)]
        let value = serde_json::from_str(CLOUD_CONFIG_SCHEMA)
            .expect("vendored cloud-config schema is valid JSON");
        Schema::new(value)
    })
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

    fn problems(instance: &serde_json::Value) -> Vec<String> {
        cloud_config_schema()
            .validate(instance)
            .into_iter()
            .map(|e| format!("{}: {}", e.path_string(), e.message))
            .collect()
    }

    #[test]
    fn the_vendored_schema_parses() {
        assert!(cloud_config_schema().as_value().is_object());
    }

    #[test]
    fn accepts_a_valid_cloud_config() {
        assert!(problems(&serde_json::json!({"runcmd": ["echo hi"]})).is_empty());
    }

    #[test]
    fn reproduces_the_documented_error_messages() {
        let got = problems(&serde_json::json!({"runcmd": 5, "bogus_key_here": 1}));
        assert!(
            got.contains(&"runcmd: 5 is not of type 'array'".to_owned()),
            "{got:?}"
        );
        assert!(
            got.iter().any(|m| m
                == ": Additional properties are not allowed ('bogus_key_here' was unexpected)"),
            "{got:?}"
        );
    }

    #[test]
    fn reports_a_bad_date_format() {
        let got = problems(&serde_json::json!({
            "users": [{"name": "u", "expiredate": "nope"}]
        }));
        assert!(
            got.contains(&"users.0.expiredate: 'nope' is not a 'date'".to_owned()),
            "{got:?}"
        );
    }

    #[test]
    fn reports_a_deprecated_key() {
        let errors = cloud_config_schema().validate(&serde_json::json!({
            "apt_reboot_if_required": true
        }));
        let deprecation = errors
            .iter()
            .find(|e| matches!(e.kind, Kind::Deprecation { .. }))
            .expect("deprecation reported");
        assert_eq!(
            deprecation.message,
            " Deprecated in version 22.2. Use **package_reboot_if_required** instead."
        );
    }
}
