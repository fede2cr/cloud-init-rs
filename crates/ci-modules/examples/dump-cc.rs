//! Runs one ported `cc_*` module against a config and dumps what landed on
//! disk, for differential testing against `tests/differential/ccmodule.py`.
//!
//! `dump-cc <cc_module> <config.json> <scratch-dir> [masked,paths]`
//!
//! Every path the fixture names is taken as written, so the harness is
//! responsible for keeping them inside the scratch directory — which it does
//! by templating the directory into the fixture, including into
//! `system_info.paths.cloud_dir` for the modules that write under it. The dump
//! is then the tree under that directory, so a module that escaped it shows up
//! as a missing file rather than as a quietly successful run.
//!
//! One key in the fixture is not config: `_datasource` describes the fake
//! datasource the module sees, overriding any of the defaults below. Setting
//! it to `null` means no datasource at all.
//!
//! The optional fourth argument names scratch-relative paths whose *content*
//! is reported as `<masked>` instead of itself, for the files that legitimately
//! differ between two runs — a timestamp, an uptime. Their presence and mode
//! are still compared.

use ci_config::Value;

/// What the fixture's `_datasource` starts from. The instance id is the one
/// modules that resolve `get_ipath` land under:
/// `<cloud_dir>/instances/i-test/`.
const DATASOURCE_DEFAULTS: &str = r#"{
    "class_name": "DataSourceNone",
    "dsname": "None",
    "instance_id": "i-test",
    "metadata": {},
    "sys_cfg": {}
}"#;

/// The fixture key describing the fake datasource.
const DATASOURCE_KEY: &str = "_datasource";

/// The fixture key asking for `root` to be the scratch directory rather than
/// `/`. Upstream has no such parameter, so its half of the harness rewrites
/// the distro attributes that name absolute files instead.
const ROOT_KEY: &str = "_root";

/// Stands in for the content of a file the harness cannot compare.
const MASKED: &str = "<masked>";

fn main() {
    let mut cli = std::env::args_os().skip(1);
    let (Some(module), Some(config), Some(scratch)) =
        (cli.next(), cli.next(), cli.next())
    else {
        eprintln!("usage: dump-cc <cc_module> <config.json> <scratch-dir> [masked]");
        std::process::exit(2);
    };
    let masked: Vec<String> = cli
        .next()
        .map(|arg| {
            arg.to_string_lossy()
                .split(',')
                .filter(|path| !path.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let module = module.to_string_lossy().into_owned();
    let Some(handler) = ci_modules::handler(&module) else {
        eprintln!("{module} has no ported body");
        std::process::exit(2);
    };
    let text = match std::fs::read_to_string(&config) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("cannot read config: {err}");
            std::process::exit(2);
        }
    };
    let Ok(Value::Object(mut cfg)) = serde_json::from_str::<Value>(&text) else {
        eprintln!("config is not a JSON object");
        std::process::exit(2);
    };
    let datasource = datasource(cfg.remove(DATASOURCE_KEY));
    let rooted = cfg
        .shift_remove(ROOT_KEY)
        .is_some_and(|value| ci_config::option::py_truthy(&value));
    let scratch = std::path::PathBuf::from(scratch);

    let paths = ci_core::Paths::from_config(&cfg);
    // `Modules.cfg` has `system_info` popped out of it; the block travels
    // beside the config, as it does in the stage driver.
    let system_info = match cfg.shift_remove("system_info") {
        Some(Value::Object(map)) => map,
        _ => ci_config::Object::new(),
    };
    let mut logger = ci_log::Logger::basic(ci_log::Level::Debug);
    let module_args = Value::Array(Vec::new());
    // The harness's Python side fakes `cloud.distro` as ubuntu; this is the
    // real object with the same answers.
    let Some(distro) = ci_distro::fetch("ubuntu") else {
        eprintln!("the distro table has no ubuntu");
        std::process::exit(2);
    };
    let mut args = ci_modules::Args {
        name: module.strip_prefix("cc_").unwrap_or(&module),
        cfg: &cfg,
        system_info: &system_info,
        args: &module_args,
        paths: &paths,
        root: if rooted {
            scratch.as_path()
        } else {
            std::path::Path::new("/")
        },
        distro,
        datasource: datasource.as_ref().map(
            |(fields, metadata, sys_cfg, public_keys)| ci_modules::Datasource {
                class_name: string(fields, "class_name"),
                dsname: string(fields, "dsname"),
                instance_id: string(fields, "instance_id"),
                metadata,
                sys_cfg,
                public_keys,
            },
        ),
        logger: &mut logger,
    };

    let outcome = handler(&mut args);

    let mut out = ci_config::Object::new();
    // Only whether it failed, not why: upstream's failure is a traceback and
    // the port's is a sentence.
    out.insert("failed".to_owned(), Value::Bool(outcome.is_err()));
    out.insert("tree".to_owned(), Value::Array(tree(&scratch, &masked)));
    println!("{}", ci_core::jsonfmt::json_dumps(&Value::Object(out)));
    if outcome.is_err() {
        std::process::exit(1);
    }
}

/// The fixture's `_datasource` over the defaults, split into the parts the
/// borrowed [`ci_modules::Datasource`] needs to point at.
#[allow(clippy::type_complexity)]
fn datasource(
    from_fixture: Option<Value>,
) -> Option<(
    ci_config::Object,
    ci_config::Object,
    ci_config::Object,
    Vec<String>,
)> {
    if matches!(from_fixture, Some(Value::Null)) {
        return None;
    }
    let Ok(Value::Object(mut fields)) =
        serde_json::from_str::<Value>(DATASOURCE_DEFAULTS)
    else {
        return None;
    };
    if let Some(Value::Object(overrides)) = from_fixture {
        fields.extend(overrides);
    }
    let object = |fields: &ci_config::Object, key: &str| match fields.get(key) {
        Some(Value::Object(map)) => map.clone(),
        _ => ci_config::Object::new(),
    };
    let metadata = object(&fields, "metadata");
    let sys_cfg = object(&fields, "sys_cfg");
    // Taken verbatim rather than derived from `metadata`, because upstream's
    // accessor is a per-cloud override and the Python side fakes it too.
    let public_keys = match fields.get("public_keys") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|key| key.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    Some((fields, metadata, sys_cfg, public_keys))
}

fn string<'a>(fields: &'a ci_config::Object, key: &str) -> &'a str {
    fields.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Every regular file under `root`, sorted, with its mode and its contents.
fn tree(root: &std::path::Path, masked: &[String]) -> Vec<Value> {
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
        .into_iter()
        .map(|(name, mode, contents)| {
            let mut map = ci_config::Object::new();
            map.insert("path".to_owned(), Value::String(name.clone()));
            map.insert("mode".to_owned(), Value::String(format!("{mode:04o}")));
            // Base64 rather than the text, so a file that is not UTF-8 — which
            // is the whole point of the `gz` encodings — still compares. A mode
            // that makes the file unreadable to the dumper is `null`, which is
            // itself worth comparing.
            let content = if masked.contains(&name) {
                Value::String(MASKED.to_owned())
            } else {
                contents.map_or(Value::Null, |bytes| {
                    Value::String(ci_core::b64::encode(&bytes))
                })
            };
            map.insert("content".to_owned(), content);
            Value::Object(map)
        })
        .collect()
}

fn walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    found: &mut Vec<(String, u32, Option<Vec<u8>>)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> =
        entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(root, &path, found);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let name = relative.to_string_lossy().into_owned();
        if meta.is_symlink() {
            // Recorded as a distinct shape so that "replaced the symlink" and
            // "wrote through it" cannot look the same.
            found.push((name, 0, Some(b"<symlink>".to_vec())));
            continue;
        }
        let mode = {
            use std::os::unix::fs::MetadataExt as _;
            meta.mode() & 0o7777
        };
        found.push((name, mode, std::fs::read(&path).ok()));
    }
}
