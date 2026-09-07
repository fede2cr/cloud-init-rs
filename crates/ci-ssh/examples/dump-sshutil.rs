//! Dumps `ssh_util.py` for the differential harness.
//!
//! Six modes, matching `tests/differential/sshutil.py`:
//!
//! * `parse <line> [options]` — one `authorized_keys` line, fully decomposed.
//! * `update <old-file> <new-keys>` — the merge, as the file that would be
//!   written. Both arguments are newline-separated (use `\n`).
//! * `paths <AuthorizedKeysFile> <homedir> <username>`
//! * `sshdcfg <content>` — the `sshd_config` line list and the derived map.
//! * `updatecfg <content> <key=value...>` — the in-place update, as the keys
//!   it reports changed and the file it would leave behind.
//! * `install <root> <username> [keys]` — pick the key file under a fixture
//!   root, optionally install `keys` into it, and dump the resulting tree.

use std::path::Path;

use ci_ssh::{config, AuthKeyLine};
use serde_json::{json, Map, Value};

fn line_json(line: &AuthKeyLine) -> Value {
    // Keys in sorted order: the Python side dumps with `sort_keys=True` and
    // `serde_json` here preserves insertion order.
    json!({
        "base64": line.base64,
        "comment": line.comment,
        "keytype": line.keytype,
        "options": line.options,
        "source": line.source,
        "str": line.to_string(),
        "valid": line.valid(),
    })
}

/// Everything under `root` except the identity databases the fixture set up,
/// as logical paths with the bits that the checks turn on.
///
/// Ownership is reported as `root` / `self` rather than as numbers: the two
/// sides run as the same unprivileged user, so the interesting question is
/// only ever whether a chown to uid 0 was attempted and failed.
fn tree(root: &Path) -> Vec<Value> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<Value>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<std::path::PathBuf> =
            entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let logical = format!("/{}", rel.display());
            if logical.starts_with("/etc") {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let kind = if meta.is_symlink() {
                "link"
            } else if meta.is_dir() {
                "dir"
            } else {
                "file"
            };
            let mode = if meta.is_symlink() {
                String::new()
            } else {
                format!("{:o}", ci_sys::ids::mode_of(&path).unwrap_or(0))
            };
            let content = if kind == "file" {
                String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default())
                    .into_owned()
            } else {
                String::new()
            };
            out.push(json!({
                "content": content,
                "kind": kind,
                "mode": mode,
                "path": logical,
            }));
            if kind == "dir" {
                walk(root, &path, out);
            }
        }
    }

    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn install(root: &Path, username: &str, keys: &[String]) -> Value {
    let mut log = ci_log::Logger::silent();
    let outcome = if keys.is_empty() {
        ci_ssh::extract_authorized_keys(root, username, ci_ssh::DEF_SSHD_CFG, &mut log)
            .map(|(chosen, _)| chosen)
    } else {
        ci_ssh::setup_user_keys(root, keys, username, "", &mut log).and_then(|()| {
            ci_ssh::extract_authorized_keys(
                root,
                username,
                ci_ssh::DEF_SSHD_CFG,
                &mut log,
            )
            .map(|(chosen, _)| chosen)
        })
    };
    match outcome {
        Ok(chosen) => json!({ "chosen": chosen, "error": false, "tree": tree(root) }),
        Err(_) => json!({ "chosen": "", "error": true, "tree": tree(root) }),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map_or("", String::as_str);
    let arg = |n: usize| args.get(n).cloned().unwrap_or_default();
    let unescape = |s: String| s.replace("\\n", "\n").replace("\\t", "\t");

    let out: Value = match mode {
        "parse" => line_json(&ci_ssh::parse_auth_key_line(&unescape(arg(1)), &arg(2))),
        "update" => {
            let old = ci_ssh::parse_authorized_keys(&unescape(arg(1)));
            let new: Vec<AuthKeyLine> = ci_core::pystr::split_lines(&unescape(arg(2)))
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(|l| ci_ssh::parse_auth_key_line(l, &arg(3)))
                .collect();
            json!({ "content": ci_ssh::update_authorized_keys(&old, &new) })
        }
        "paths" => json!({
            "paths": ci_ssh::render_authorizedkeysfile_paths(&arg(1), &arg(2), &arg(3)),
        }),
        "sshdcfg" => {
            let text = unescape(arg(1));
            let raw = ci_core::pystr::split_lines(&text);
            let lines = config::parse_config_lines(&raw, &mut ci_log::Logger::silent());
            let rendered: Vec<Value> = lines
                .iter()
                .map(|l| {
                    json!({
                        "key": l.key(),
                        "raw_key": l.raw_key(),
                        "str": l.render(),
                        "value": l.value,
                    })
                })
                .collect();
            let mut map = Map::new();
            for (k, v) in config::config_map(&lines) {
                map.insert(k, Value::String(v));
            }
            json!({ "lines": rendered, "map": Value::Object(map) })
        }
        "updatecfg" => {
            let text = unescape(arg(1));
            let raw = ci_core::pystr::split_lines(&text);
            let mut lines =
                config::parse_config_lines(&raw, &mut ci_log::Logger::silent());
            // `key=value`, split on the first `=` so a value may contain one.
            let spec = unescape(arg(2));
            let updates: Vec<(&str, &str)> = ci_core::pystr::split_lines(&spec)
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(|l| l.split_once('=').unwrap_or((l, "")))
                .collect();
            let changed = ci_ssh::update_ssh_config_lines(
                &mut lines,
                &updates,
                &mut ci_log::Logger::silent(),
            );
            let body = lines
                .iter()
                .map(config::SshdConfigLine::render)
                .collect::<Vec<_>>()
                .join("\n");
            json!({ "changed": changed, "content": format!("{body}\n") })
        }
        "install" => {
            let keys: Vec<String> = ci_core::pystr::split_lines(&unescape(arg(3)))
                .into_iter()
                .filter(|l| !l.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            install(Path::new(&arg(1)), &arg(2), &keys)
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    };

    println!("{}", ci_core::jsonfmt::dumps_indent(&out, 1));
}
