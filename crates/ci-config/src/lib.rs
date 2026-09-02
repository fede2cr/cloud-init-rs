//! cloud-config loading, merging and the `cloud.cfg` / `cloud.cfg.d` pipeline.
//!
//! Merge semantics are a deliberate, behaviour-for-behaviour port of
//! `cloudinit/mergers/*` and `cloudinit/util.py::mergemanydict`, including the
//! non-obvious default (`list()+dict()+str()`, i.e. *first value wins* for
//! conflicting keys). Getting this wrong silently mis-provisions instances, so the
//! rules are reproduced exactly and covered by tests derived from upstream.

pub mod builtin;
pub mod cmdline;
pub mod merge;
pub mod read;
pub mod yaml;

pub use merge::{MergerSpec, Mergers};
pub use yaml::{load_yaml, Limits, YamlError};

/// Canonical in-memory config value.
///
/// `serde_json::Value` (with `preserve_order`) is used rather than a YAML value so
/// that config, instance-data and JSON output all share one representation and
/// mapping order survives round-trips, matching Python dict ordering.
pub type Value = serde_json::Value;

/// A cloud-config mapping.
pub type Object = serde_json::Map<String, Value>;
