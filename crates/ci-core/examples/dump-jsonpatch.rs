//! Apply a `#cloud-config-jsonp` patch and dump the result, as JSON.
//!
//! Reads `{"doc": <document>, "patch": <patch as a string>}` on stdin. Exists
//! so `ci_config::jsonpatch` can be diffed against the `jsonpatch` library;
//! see tests/differential/jsonpatch.py.
//!
//! Lives in `ci-core` rather than `ci-config` for the Python-compatible JSON
//! writer, which is what makes the two sides' output comparable at all.
//!
//! Failures are reported by class, not by message: upstream's messages are only
//! ever logged, and the port deliberately does not reproduce them. What has to
//! agree is whether Python would have raised a `ValueError`, because
//! `CloudConfigPartHandler` records those parts and drops the rest.

use std::io::Read as _;

use ci_config::jsonpatch::Patch;
use ci_config::Value;

fn main() {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("read: {e}");
        std::process::exit(1);
    }
    let case: Value = match serde_json::from_str(&input) {
        Ok(case) => case,
        Err(e) => {
            eprintln!("parse: {e}");
            std::process::exit(1);
        }
    };
    let doc = case.get("doc").cloned().unwrap_or(Value::Null);
    let patch = case
        .get("patch")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let outcome = match Patch::parse(patch).and_then(|patch| patch.apply(&doc)) {
        Ok(result) => serde_json::json!({ "ok": result }),
        Err(e) => serde_json::json!({
            "error": if e.is_value_error() { "value" } else { "failed" },
        }),
    };
    println!("{}", ci_core::jsonfmt::dumps_indent(&outcome, 2));
}
