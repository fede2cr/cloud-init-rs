//! Port of `cloudinit/config/cc_seed_random.py`.
//!
//! Appends entropy to a file — `/dev/urandom` by default, so the normal case
//! is a write to the kernel's pool rather than to a file at all — and then
//! optionally runs a command with `RANDOM_SEED_FILE` in its environment, which
//! is how a distro re-seeds `systemd-random-seed` or `rngd`.
//!
//! Two sources are concatenated in a fixed order: the `random_seed.data` the
//! *config* carries, then the `random_seed` the *datasource* carries. Neither
//! is treated as a secret by this module — the config it came from is on disk
//! already — but the target file is appended to, never truncated, so an
//! existing pool is added to rather than replaced.

use ci_config::{Object, Value};
use ci_sys::subp::Subp;

use super::Args;

const SOURCE: &str = "cc_seed_random.py";
const DEFAULT_FILE: &str = "/dev/urandom";

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mycfg = match args.cfg.get("random_seed") {
        Some(Value::Object(map)) => map.clone(),
        // Upstream's `cfg.get("random_seed", {})` then `.get(...)` on whatever
        // came back; a non-mapping raises AttributeError.
        Some(other) => {
            return Err(format!(
                "'{}' object has no attribute 'get'",
                ci_config::type_name(other)
            ))
        }
        None => Object::new(),
    };

    let seed_path = match mycfg.get("file") {
        Some(Value::String(path)) => path.clone(),
        None | Some(Value::Null) => DEFAULT_FILE.to_owned(),
        Some(other) => super::py_str(other),
    };

    let mut seed = Vec::new();
    if let Some(data) = truthy(mycfg.get("data")) {
        seed.extend_from_slice(&decode(data, mycfg.get("encoding"))?);
    }
    if let Some(from_metadata) = args
        .datasource
        .and_then(|ds| ds.metadata.get("random_seed"))
    {
        seed.extend_from_slice(encode_text(from_metadata).as_bytes());
    }

    if !seed.is_empty() {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!(
                "{name}: adding {} bytes of random seed entropy to {seed_path}",
                seed.len()
            ),
        );
        // `util.append_file` passes `mode=None`, so an existing file keeps its
        // mode and a new one gets 0o666 less the umask.
        ci_sys::atomic::append_file(&seed_path, &seed, 0o666)
            .map_err(|err| format!("Failed to append to file {seed_path}: {err}"))?;
    }

    let command = mycfg.get("command");
    let required = matches!(mycfg.get("command_required"), Some(Value::Bool(true)));
    run_command(args, command, required, &seed_path).map_err(|err| {
        let shown = command.map_or_else(|| "None".to_owned(), ci_config::repr);
        args.warning(
            SOURCE,
            &format!("handling random command [{shown}] failed: {err}"),
        );
        err
    })
}

/// `handle_random_seed_command`.
///
/// `command_required` is the whole point of the function: without it a missing
/// re-seed binary is a debug line and the boot continues with whatever entropy
/// the pool already had, which on a fresh cloud image can be very little.
fn run_command(
    args: &mut Args<'_>,
    command: Option<&Value>,
    required: bool,
    seed_path: &str,
) -> Result<(), String> {
    let items = match command {
        Some(Value::Array(items)) if !items.is_empty() => items,
        _ => {
            if required {
                return Err("no command found but required=true".to_owned());
            }
            args.debug(SOURCE, "no command provided");
            return Ok(());
        }
    };
    // Upstream indexes the value it was handed, whatever it is; a list of
    // non-strings gets `str()`d by subp anyway. The list is known non-empty
    // from the guard above, so the `else` is only there to avoid indexing.
    let Some(program) = items.first().map(super::py_str) else {
        return Ok(());
    };
    if ci_sys::subp::which(&program).is_none() {
        if required {
            return Err(format!("command '{program}' not found but required=true"));
        }
        args.debug(
            SOURCE,
            &format!("command '{program}' not found for seed_command"),
        );
        return Ok(());
    }

    let cmd: Vec<String> = items.iter().map(super::py_str).collect();
    let status = Subp::new(&cmd)
        .inherit_env()
        .env("RANDOM_SEED_FILE", seed_path)
        .passthrough()
        .map_err(|err| err.to_string())?;
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "Unexpected error while running command. Command: {cmd:?}"
    ))
}

/// `_decode`: `raw` and an absent encoding pass through, `b64`/`base64`
/// decode, `gz`/`gzip` decompress, anything else is an error.
///
/// Deliberately narrower than `write_files`' encoding handling, which accepts
/// combined spellings like `gz+b64`. Upstream's two tables genuinely differ
/// and a config that works for one key does not work for the other.
fn decode(data: &Value, encoding: Option<&Value>) -> Result<Vec<u8>, String> {
    let text = encode_text(data);
    let encoding = match encoding {
        None | Some(Value::Null) => String::new(),
        Some(other) => super::py_str(other),
    };
    match encoding.to_ascii_lowercase().as_str() {
        "" | "raw" => Ok(text.into_bytes()),
        "base64" | "b64" => ci_core::b64::decode(&text)
            .ok_or_else(|| "Invalid base64-encoded string".to_owned()),
        "gzip" | "gz" => ci_core::gzip::decompress(text.as_bytes())
            .map_err(|err| format!("Failed to decompress random_seed: {err}")),
        other => Err(format!("Unknown random_seed encoding: {other}")),
    }
}

/// `util.encode_text` after the config parser has already produced a `str`.
fn encode_text(value: &Value) -> String {
    super::py_str(value)
}

/// Python's `if seed_data:` — an empty string and a missing key are the same
/// thing here, and so are `0` and `false`.
fn truthy(value: Option<&Value>) -> Option<&Value> {
    match value? {
        Value::Null | Value::Bool(false) => None,
        Value::String(text) if text.is_empty() => None,
        Value::Array(items) if items.is_empty() => None,
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        other => Some(other),
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
    use std::path::Path;

    use ci_log::Logger;
    use serde_json::json;

    use super::*;

    fn run(
        root: &Path,
        cfg: &serde_json::Value,
        metadata: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        let cfg = cfg.as_object().unwrap();
        let metadata =
            metadata.map_or_else(Object::new, |m| m.as_object().unwrap().clone());
        let sys_cfg = Object::new();
        let paths = ci_core::Paths {
            cloud_dir: root.join("cloud"),
            ..Default::default()
        };
        let mut logger = Logger::silent();
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "seed_random",
            cfg,
            args: &empty,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(crate::cc::Datasource {
                class_name: "DataSourceNoCloud",
                dsname: "NoCloud",
                instance_id: "i-test",
                metadata: &metadata,
                sys_cfg: &sys_cfg,
                public_keys: &[],
            }),
            logger: &mut logger,
        };
        handle(&mut args)
    }

    #[test]
    fn without_data_or_metadata_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let cfg = json!({"random_seed": {"file": seed.to_str().unwrap()}});
        assert_eq!(run(dir.path(), &cfg, None), Ok(()));
        assert!(!seed.exists());
    }

    #[test]
    fn config_data_comes_first_and_metadata_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let cfg = json!({
            "random_seed": {"file": seed.to_str().unwrap(), "data": "from-cfg"}
        });
        assert_eq!(
            run(dir.path(), &cfg, Some(&json!({"random_seed": "from-md"}))),
            Ok(())
        );
        assert_eq!(std::fs::read_to_string(&seed).unwrap(), "from-cfgfrom-md");
    }

    #[test]
    fn an_existing_pool_is_added_to_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::write(&seed, "old").unwrap();
        let cfg = json!({
            "random_seed": {"file": seed.to_str().unwrap(), "data": "new"}
        });
        assert_eq!(run(dir.path(), &cfg, None), Ok(()));
        assert_eq!(std::fs::read_to_string(&seed).unwrap(), "oldnew");
    }

    #[test]
    fn the_encodings_are_narrower_than_write_files() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let cfg = json!({"random_seed": {
            "file": seed.to_str().unwrap(),
            "data": "cGxhaW4gcGF5bG9hZAo=",
            "encoding": "B64",
        }});
        assert_eq!(run(dir.path(), &cfg, None), Ok(()));
        assert_eq!(std::fs::read_to_string(&seed).unwrap(), "plain payload\n");

        let cfg = json!({"random_seed": {
            "file": seed.to_str().unwrap(),
            "data": "x",
            "encoding": "gz+b64",
        }});
        assert_eq!(
            run(dir.path(), &cfg, None),
            Err("Unknown random_seed encoding: gz+b64".to_owned())
        );
    }

    #[test]
    fn a_missing_command_is_only_fatal_when_it_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let base = json!({"file": seed.to_str().unwrap(), "data": "x"});
        let mut cfg = base.as_object().unwrap().clone();
        cfg.insert("command".to_owned(), json!(["definitely-not-a-program"]));
        assert_eq!(
            run(dir.path(), &json!({"random_seed": cfg.clone()}), None),
            Ok(())
        );
        cfg.insert("command_required".to_owned(), json!(true));
        assert_eq!(
            run(dir.path(), &json!({"random_seed": cfg}), None),
            Err(
                "command 'definitely-not-a-program' not found but required=true"
                    .to_owned()
            )
        );
    }

    #[test]
    fn an_absent_command_is_required_too() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = json!({"random_seed": {"command_required": true}});
        assert_eq!(
            run(dir.path(), &cfg, None),
            Err("no command found but required=true".to_owned())
        );
    }

    #[test]
    fn the_command_sees_the_seed_file_in_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let out = dir.path().join("out");
        let cfg = json!({"random_seed": {
            "file": seed.to_str().unwrap(),
            "data": "x",
            "command": ["/bin/sh", "-c",
                        format!("printf %s \"$RANDOM_SEED_FILE\" > {}",
                                out.display())],
        }});
        assert_eq!(run(dir.path(), &cfg, None), Ok(()));
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            seed.to_str().unwrap()
        );
    }

    #[test]
    fn a_random_seed_that_is_not_a_mapping_fails_the_module() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run(dir.path(), &json!({"random_seed": []}), None),
            Err("'list' object has no attribute 'get'".to_owned())
        );
    }
}
