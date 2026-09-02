//! Python-compatible JSON serialisation.
//!
//! Upstream emits `json.dumps(obj, indent=N, sort_keys=True, separators=(",", ": "))`.
//! `serde_json`'s pretty printer defaults to two-space indentation and preserves
//! insertion order, so both the indent width and the key ordering have to be
//! matched explicitly for output to be diffable against Python cloud-init.

use serde::Serialize;
use serde_json::ser::PrettyFormatter;
use serde_json::{Serializer, Value};

/// `util.json_dumps()` — indent 1, sorted keys.
pub fn json_dumps(value: &Value) -> String {
    dumps_indent(value, 1)
}

/// `json.dumps(..., indent=n, sort_keys=True, separators=(",", ": "))`.
pub fn dumps_indent(value: &Value, indent: usize) -> String {
    let sorted = sort_keys(value);
    let pad = " ".repeat(indent);
    let mut buf = Vec::new();
    let formatter = PrettyFormatter::with_indent(pad.as_bytes());
    let mut ser = Serializer::with_formatter(&mut buf, formatter);
    if sorted.serialize(&mut ser).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Recursively reorder mappings by key, emulating `sort_keys=True`.
pub fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for key in keys {
                if let Some(v) = map.get(key) {
                    out.insert(key.clone(), sort_keys(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
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
    use serde_json::json;

    #[test]
    fn matches_python_indent_one_sorted() {
        let value = json!({"b": 2, "a": {"d": 4, "c": [1, 2]}});
        assert_eq!(
            json_dumps(&value),
            "{\n \"a\": {\n  \"c\": [\n   1,\n   2\n  ],\n  \"d\": 4\n },\n \"b\": 2\n}"
        );
    }

    #[test]
    fn supports_indent_two_for_status_output() {
        let value = json!({"status": "done"});
        assert_eq!(dumps_indent(&value, 2), "{\n  \"status\": \"done\"\n}");
    }
}
