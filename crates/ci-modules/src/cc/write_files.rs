//! Port of `cloudinit/config/cc_write_files.py`.
//!
//! The module that writes tenant-supplied bytes to tenant-supplied paths as
//! root. Everything it does is worth being exact about, so the awkward corners
//! are reproduced rather than tidied: the entry numbering in the warnings
//! starts at one, a `path` that is missing is a warning and not a failure, a
//! `source` that cannot be fetched falls back to `content` rather than
//! aborting, and an entry with neither writes an empty file.
//!
//! Two things are deliberately *not* reproduced; both are recorded in
//! docs/COMPAT.md.

use std::path::{Path, PathBuf};

use ci_config::{repr, Object, Value};

use super::Args;

/// Log source, matching `logging.getLogger(__name__)` in the Python module.
const SOURCE: &str = "cc_write_files.py";

/// `cc_write_files.DEFAULT_PERMS`.
const DEFAULT_PERMS: i64 = 0o644;

/// `cc_write_files.DEFAULT_DEFER`.
pub(super) const DEFAULT_DEFER: bool = false;

/// `cc_write_files.TEXT_PLAIN_ENC`.
const TEXT_PLAIN_ENC: &str = "text/plain";

/// How hard to try a `source.uri`, at upstream's "arbitrarily chosen" values.
const URL_RETRIES: u32 = 3;
const URL_SEC_BETWEEN: std::time::Duration = std::time::Duration::from_secs(3);

/// `cc_write_files.handle`.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let cfg = args.cfg;

    // `defer: true` belongs to cc_write_files_deferred, which runs in the
    // final stage. Filtering happens before the emptiness check, so a config
    // whose every entry is deferred logs the same "no/empty" line as one with
    // no key at all.
    let filtered: Vec<&Value> = entries(cfg)?
        .into_iter()
        .filter(|entry| !option_bool(entry, "defer", DEFAULT_DEFER))
        .collect();
    if filtered.is_empty() {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, no/empty 'write_files' key in configuration"
            ),
        );
        return Ok(());
    }

    let ssl = ci_url::fetch_ssl_details(args.paths);
    let owner = args.distro.default_owner;
    write_files(args, &filtered, owner, &ssl)
}

/// The `write_files` list, shared with `cc_write_files_deferred`.
pub(super) fn entries(cfg: &Object) -> Result<Vec<&Value>, String> {
    match cfg.get("write_files") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.iter().collect()),
        // Upstream iterates whatever it finds. A mapping yields its keys and a
        // string its characters, and both then die on `.get`; a number is not
        // iterable at all. Every one of those is a traceback, so this is one
        // too — just a legible one.
        Some(other) => Err(format!(
            "'write_files' must be a list, not {}",
            ci_config::type_name(other)
        )),
    }
}

/// `cc_write_files.write_files`.
pub(super) fn write_files(
    args: &mut Args<'_>,
    files: &[&Value],
    owner: &str,
    ssl: &ci_url::SslDetails,
) -> Result<(), String> {
    for (index, entry) in files.iter().enumerate() {
        let number = index + 1;
        let Some(entry) = entry.as_object() else {
            return Err(format!(
                "entry {number} of 'write_files' must be a mapping, not {}",
                ci_config::type_name(entry)
            ));
        };

        let path = entry.get("path").and_then(as_present_str);
        let Some(path) = path.filter(|text| !text.is_empty()) else {
            let name = args.name.to_owned();
            args.warning(
                SOURCE,
                &format!(
                    "No path provided to write for entry {number} in module {name}"
                ),
            );
            continue;
        };
        let path = abspath(path);

        let contents = read_url_or_decode(
            args,
            entry.get("source"),
            ssl,
            entry.get("content"),
            entry.get("encoding"),
        )?;
        let Some(contents) = contents else {
            let name = args.name.to_owned();
            args.warning(
                SOURCE,
                &format!(
                    "No content could be loaded for entry {number} in module {name}; skipping"
                ),
            );
            continue;
        };

        let configured_owner = entry.get("owner").and_then(as_present_str);
        let (user, group) = extract_usergroup(configured_owner.unwrap_or(owner));
        let perms = decode_perms(args, entry.get("permissions"), DEFAULT_PERMS);
        let append = ci_config::option::get_bool(entry, "append", false);

        write_one(
            args,
            &path,
            &contents,
            perms,
            append,
            user.as_deref(),
            group.as_deref(),
        )?;
    }
    Ok(())
}

/// `util.write_file` followed by the `chownbyname`/`chmod` pair that
/// `write_files` does itself.
///
/// The order is upstream's and it is not redundant: the write applies the
/// mode, the chown can clear the setuid and setgid bits as a side effect, and
/// the final chmod puts them back only if they actually went missing.
fn write_one(
    args: &mut Args<'_>,
    path: &Path,
    contents: &[u8],
    perms: i64,
    append: bool,
    user: Option<&str>,
    group: Option<&str>,
) -> Result<(), String> {
    let mode = u32::try_from(perms)
        .map_err(|_| format!("Invalid permissions {perms} for {}", path.display()))?;

    let root = args.root.to_path_buf();
    if let Some(parent) = path.parent() {
        ensure_dir(&root, parent, user, group)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }

    // `util.chmod` skips a falsy mode, so `permissions: 0` never reaches a
    // `chmod` at all and the file keeps whatever `open()` gave it: 0o666 with
    // the umask applied. Creating it the same way reproduces that, umask and
    // all, rather than producing an unreadable file.
    let create_mode = if mode == 0 { 0o666 } else { mode };

    let describe = format!(
        "Writing to {} - {}: [{mode:o}] {} bytes",
        path.display(),
        if append { "ab" } else { "wb" },
        contents.len()
    );
    args.debug(SOURCE, &describe);

    if append {
        ci_sys::atomic::append_file(path, contents, create_mode)
    } else {
        ci_sys::atomic::write_file(
            path,
            contents,
            ci_sys::atomic::WriteOptions::mode(create_mode),
        )
    }
    .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;

    if mode != 0 {
        ci_sys::ids::set_mode(path, mode)
            .map_err(|e| format!("Failed to chmod {}: {e}", path.display()))?;
    }
    ci_sys::ids::chown_by_name(&root, path, user, group)
        .map_err(|e| format!("Failed to chown {}: {e}", path.display()))?;
    // The chown can clear the setuid and setgid bits as a side effect; this
    // puts them back, and only then.
    if mode != 0 && ci_sys::ids::mode_of(path).is_ok_and(|actual| actual != mode) {
        ci_sys::ids::set_mode(path, mode)
            .map_err(|e| format!("Failed to chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// `util.ensure_dir`, restricted to what `write_file` asks of it: create the
/// missing components, and hand each newly created one to the same owner the
/// file is going to get.
fn ensure_dir(
    root: &Path,
    dir: &Path,
    user: Option<&str>,
    group: Option<&str>,
) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(current) = cursor {
        if current.exists() {
            break;
        }
        missing.push(current.to_path_buf());
        cursor = current.parent();
    }
    std::fs::create_dir_all(dir)?;
    if user.is_none() && group.is_none() {
        return Ok(());
    }
    for created in missing.iter().rev() {
        ci_sys::ids::chown_by_name(root, created, user, group)?;
    }
    Ok(())
}

/// `cc_write_files.read_url_or_decode`.
///
/// `Ok(None)` is upstream's `None` return: a `source` that could not be
/// fetched and no `content` to fall back on.
fn read_url_or_decode(
    args: &mut Args<'_>,
    source: Option<&Value>,
    ssl: &ci_url::SslDetails,
    content: Option<&Value>,
    encoding: Option<&Value>,
) -> Result<Option<Vec<u8>>, String> {
    let url = match source {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => map.get("uri").and_then(as_present_str),
        Some(other) => {
            return Err(format!(
                "'source' must be a mapping, not {}",
                ci_config::type_name(other)
            ))
        }
    };
    let content = content.filter(|value| !matches!(value, Value::Null));

    // The one case with no work to do: nothing to fetch and nothing to
    // decode, which upstream spells "write a blank file".
    if content.is_none() && url.is_none() {
        return Ok(Some(Vec::new()));
    }

    let mut result = None;
    let mut used_url = false;
    if let Some(url) = url {
        let headers = source
            .and_then(Value::as_object)
            .and_then(|map| map.get("headers"))
            .map_or_else(Vec::new, header_pairs);
        let config = ci_url::Config {
            retries: URL_RETRIES,
            sec_between: URL_SEC_BETWEEN,
            headers,
            ssl: ssl.clone(),
            ..ci_url::Config::default()
        };
        match ci_url::read_file_or_url(url, &config) {
            Ok(response) => {
                result = Some(response.contents);
                used_url = true;
            }
            Err(e) => {
                args.warning(
                    SOURCE,
                    &format!(
                        "Failed to retrieve contents from source \"{url}\"; falling back to \
                         data from \"contents\" key: {e}"
                    ),
                );
            }
        }
    }

    // Not an `else`: `used_url` is false both when no URL was given and when
    // the one that was given failed.
    if !used_url {
        if let Some(content) = content {
            let Some(text) = content.as_str() else {
                return Err(format!(
                    "'content' must be a string, not {}",
                    ci_config::type_name(content)
                ));
            };
            result = Some(extract_contents(
                text.as_bytes(),
                &canonicalize_extraction(args, encoding)?,
            )?);
        }
    }
    Ok(result)
}

/// One step of `cc_write_files.extract_contents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extraction {
    Gzip,
    Base64,
    Plain,
}

/// `cc_write_files.canonicalize_extraction`.
fn canonicalize_extraction(
    args: &mut Args<'_>,
    encoding: Option<&Value>,
) -> Result<Vec<Extraction>, String> {
    let raw = match encoding {
        None | Some(Value::Null) => return Ok(vec![Extraction::Plain]),
        Some(Value::String(text)) => text.clone(),
        Some(other) => {
            return Err(format!(
                "'encoding' must be a string, not {}",
                ci_config::type_name(other)
            ))
        }
    };
    let kind = raw.to_lowercase();
    let kind = kind.trim();
    Ok(match kind {
        // An empty encoding and an explicit `text/plain` are the same request.
        "" | TEXT_PLAIN_ENC => vec![Extraction::Plain],
        "gz" | "gzip" => vec![Extraction::Gzip],
        "gz+base64" | "gzip+base64" | "gz+b64" | "gzip+b64" => {
            vec![Extraction::Base64, Extraction::Gzip]
        }
        "b64" | "base64" => vec![Extraction::Base64],
        other => {
            let message =
                format!("Unknown encoding type {other}, assuming {TEXT_PLAIN_ENC}");
            args.warning(SOURCE, &message);
            vec![Extraction::Plain]
        }
    })
}

/// `cc_write_files.extract_contents`.
fn extract_contents(
    contents: &[u8],
    extractions: &[Extraction],
) -> Result<Vec<u8>, String> {
    let mut result = contents.to_vec();
    for step in extractions {
        result = match step {
            // `decomp_gzip(quiet=False)`: a bad stream fails the module.
            Extraction::Gzip => ci_core::gzip::decompress(&result)?,
            Extraction::Base64 => {
                let text = std::str::from_utf8(&result)
                    .map_err(|_| "Invalid base64-encoded string".to_owned())?;
                ci_core::b64::decode(text)
                    .ok_or_else(|| "Invalid base64-encoded string".to_owned())?
            }
            Extraction::Plain => result,
        };
    }
    Ok(result)
}

/// `cc_write_files.decode_perms`.
///
/// Never fails: an undecodable value is a warning and the default, because a
/// typo in `permissions` should not stop the rest of the entries from being
/// written.
fn decode_perms(args: &mut Args<'_>, perm: Option<&Value>, default: i64) -> i64 {
    let decoded = match perm {
        None | Some(Value::Null) => return default,
        // Python has no separate bool: `permissions: true` is `int(True)`, 1.
        Some(Value::Bool(flag)) => Some(i64::from(*flag)),
        Some(Value::Number(number)) => number.as_i64().or_else(|| {
            // `int()` on a float truncates towards zero. Python would carry a
            // value too large for an i64 as a bignum and only fail later, at
            // `chmod`; refusing it here reaches the same place by the shorter
            // route, which is the warning and the default.
            number
                .as_f64()
                .filter(|value| value.is_finite() && value.abs() < 9.0e18)
                .map(|value| {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        value.trunc() as i64
                    }
                })
        }),
        // `int(str(perm), 8)`. `str` of a string is the string; of a list or a
        // mapping it is the repr, which never parses as octal.
        Some(value) => parse_octal(
            &value
                .as_str()
                .map_or_else(|| repr(value), std::borrow::ToOwned::to_owned),
        ),
    };
    decoded.unwrap_or_else(|| {
        // Upstream tries `"%o" % perm` first and falls back to `"%r" % perm`.
        // Only a string, list or mapping gets this far, and none of those can
        // be formatted as an integer, so it is always the repr.
        let shown = perm.map_or_else(|| "None".to_owned(), repr);
        args.warning(
            SOURCE,
            &format!("Undecodable permissions {shown}, returning default {default:o}"),
        );
        default
    })
}

/// `int(text, 8)`: optional sign, an optional `0o` prefix, and underscores
/// between digits.
fn parse_octal(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = digits
        .strip_prefix("0o")
        .or_else(|| digits.strip_prefix("0O"))
        .unwrap_or(digits);
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut value: i64 = 0;
    let mut previous_underscore = false;
    for c in digits.chars() {
        if c == '_' {
            if previous_underscore {
                return None;
            }
            previous_underscore = true;
            continue;
        }
        previous_underscore = false;
        let digit = c.to_digit(8)?;
        value = value.checked_mul(8)?.checked_add(i64::from(digit))?;
    }
    Some(if negative { -value } else { value })
}

/// `util.extract_usergroup`.
fn extract_usergroup(pair: &str) -> (Option<String>, Option<String>) {
    if pair.is_empty() {
        return (None, None);
    }
    let (user, group) = match pair.split_once(':') {
        Some((user, group)) => (user.trim(), Some(group.trim())),
        None => (pair.trim(), None),
    };
    let keep = |name: &str| {
        if name.is_empty() || name == "-1" || name.eq_ignore_ascii_case("none") {
            None
        } else {
            Some(name.to_owned())
        }
    };
    (keep(user), group.and_then(keep))
}

/// `util.get_cfg_option_bool` against a value that may not be a mapping at
/// all. Upstream's `key not in yobj` raises for a number and does a substring
/// test on a string; neither reaches here, because the caller has already
/// rejected the shapes that would.
pub(super) fn option_bool(value: &Value, key: &str, default: bool) -> bool {
    value.as_object().map_or(default, |map| {
        ci_config::option::get_bool(map, key, default)
    })
}

/// A string value, or `None` when the key is absent or null. A non-string is
/// also `None`: upstream would reach an `AttributeError` a line or two later,
/// and the caller here treats "no usable value" the same way.
fn as_present_str(value: &Value) -> Option<&str> {
    value.as_str()
}

/// `source.headers` as `readurl` wants them. Non-string values are dropped
/// rather than stringified, so a nested mapping cannot become a header.
fn header_pairs(value: &Value) -> Vec<(String, String)> {
    let Value::Object(map) = value else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, value)| {
            value.as_str().map(|text| (name.clone(), text.to_owned()))
        })
        .collect()
}

/// `os.path.abspath`: join with the working directory if relative, then
/// normalise lexically. Symlinks are not resolved — that is `realpath` — so a
/// `..` here removes the preceding component whether or not it was a link,
/// which is exactly the behaviour being reproduced.
fn abspath(path: &str) -> PathBuf {
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        format!("{}/{path}", cwd.display())
    };
    normpath(&joined)
}

/// `posixpath.normpath`.
fn normpath(path: &str) -> PathBuf {
    // POSIX says a path starting with exactly two slashes is implementation
    // defined, and `normpath` preserves them. Nothing in this port creates
    // such a path, but nothing filters one out either.
    let leading = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if leading.is_empty() {
                    // A relative path may keep leading `..` components; an
                    // absolute one cannot go above the root.
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if leading.is_empty() {
        PathBuf::from(if joined.is_empty() {
            ".".to_owned()
        } else {
            joined
        })
    } else {
        PathBuf::from(format!("{leading}{joined}"))
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
    use ci_log::Logger;
    use serde_json::json;

    /// Drive `handle` against a config, with every path already rooted in a
    /// scratch directory. `write_files` is one of the few modules that can be
    /// exercised for real without touching the host: the paths it writes to
    /// are the ones the config names.
    fn run(root: &Path, cfg: &serde_json::Value) -> Result<(), String> {
        let Some(cfg) = cfg.as_object() else {
            panic!("config fixture must be a mapping");
        };
        passwd_fixture(root);
        let paths = ci_core::Paths::default();
        let mut logger = Logger::silent();
        let args_value = Value::Null;
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "write_files",
            cfg,
            args: &args_value,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(crate::cc::tests::fixture_datasource()),
            logger: &mut logger,
        };
        handle(&mut args)
    }

    /// An `/etc/passwd` and `/etc/group` in which `root` is *this* user.
    ///
    /// Every entry is chowned to `root:root` unless it says otherwise, and a
    /// test process is not root. Pointing the name at the current ids runs the
    /// same code — resolve the name, apply it — through a `chown` the kernel
    /// will allow.
    fn passwd_fixture(root: &Path) {
        use std::os::unix::fs::MetadataExt as _;
        let meta = std::fs::metadata(root).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/passwd"),
            format!("root:x:{}:{}::/root:/bin/sh\n", meta.uid(), meta.gid()),
        )
        .unwrap();
        std::fs::write(root.join("etc/group"), format!("root:x:{}:\n", meta.gid()))
            .unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_config_without_the_key_is_a_skip_and_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), &json!({})), Ok(()));
        assert_eq!(run(dir.path(), &json!({"write_files": []})), Ok(()));
        // Every entry deferred is the same as no entries at all.
        assert_eq!(
            run(
                dir.path(),
                &json!({"write_files": [{"path": "/nope", "defer": true}]})
            ),
            Ok(())
        );
        assert!(!dir.path().join("nope").exists());
    }

    #[test]
    fn content_lands_with_the_requested_mode_and_missing_parents_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("deep/er/still/hello.txt");
        run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "content": "hello\n",
                "permissions": "0600",
            }]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello\n");
        assert_eq!(mode_of(&target), 0o600);
    }

    #[test]
    fn an_entry_with_neither_source_nor_content_writes_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("blank");
        run(
            dir.path(),
            &json!({"write_files": [{"path": target.to_str().unwrap()}]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"");
        // The documented default, applied because nothing else was asked for.
        assert_eq!(mode_of(&target), 0o644);
    }

    #[test]
    fn append_adds_to_what_is_already_there_and_the_default_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("log");
        let path = target.to_str().unwrap();
        run(
            dir.path(),
            &json!({"write_files": [
                {"path": path, "content": "one\n"},
                {"path": path, "content": "two\n", "append": true},
                {"path": path, "content": "three\n", "append": "yes"},
            ]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"one\ntwo\nthree\n");

        run(
            dir.path(),
            &json!({"write_files": [{"path": path, "content": "fresh\n"}]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"fresh\n");
    }

    #[test]
    fn encoded_content_is_decoded_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let plain = b"secret payload\n";
        let gz = {
            use std::io::Write as _;
            let mut encoder = flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            );
            encoder.write_all(plain).unwrap();
            encoder.finish().unwrap()
        };
        let b64 = ci_core::b64::encode(plain);
        let gz_b64 = ci_core::b64::encode(&gz);

        for (encoding, content) in [
            ("b64", b64.as_str()),
            ("base64", b64.as_str()),
            ("gz+b64", gz_b64.as_str()),
            ("gzip+base64", gz_b64.as_str()),
        ] {
            let target = dir.path().join(format!("out-{encoding}"));
            run(
                dir.path(),
                &json!({"write_files": [{
                    "path": target.to_str().unwrap(),
                    "encoding": encoding,
                    "content": content,
                }]}),
            )
            .unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), plain, "{encoding}");
        }
    }

    #[test]
    fn an_unknown_encoding_falls_back_to_plain_text_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("plain");
        run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "encoding": "rot13",
                "content": "aGk=",
            }]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"aGk=");
    }

    #[test]
    fn a_broken_encoding_fails_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bad");
        let err = run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "encoding": "gzip",
                "content": "not gzip at all",
            }]}),
        )
        .unwrap_err();
        assert!(!err.is_empty());
        assert!(!target.exists());
    }

    #[test]
    fn an_entry_without_a_path_is_skipped_and_the_rest_still_run() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("after");
        run(
            dir.path(),
            &json!({"write_files": [
                {"content": "orphan\n"},
                {"path": "", "content": "also orphan\n"},
                {"path": target.to_str().unwrap(), "content": "kept\n"},
            ]}),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"kept\n");
    }

    #[test]
    fn undecodable_permissions_fall_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("perm");
        run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "content": "x",
                "permissions": "not-a-mode",
            }]}),
        )
        .unwrap();
        assert_eq!(mode_of(&target), 0o644);
    }

    #[test]
    fn a_write_files_key_of_the_wrong_shape_fails_the_module() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run(dir.path(), &json!({"write_files": "nope"})).is_err());
        assert!(run(dir.path(), &json!({"write_files": [7]})).is_err());
    }

    #[test]
    fn a_symlink_at_the_target_is_replaced_rather_than_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, b"untouched").unwrap();
        let target = dir.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();

        run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "content": "new\n",
            }]}),
        )
        .unwrap();
        // COMPAT.md: upstream follows the link and clobbers `elsewhere`.
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"untouched");
        assert_eq!(std::fs::read(&target).unwrap(), b"new\n");
        assert!(!std::fs::symlink_metadata(&target).unwrap().is_symlink());
    }

    #[test]
    fn an_unknown_owner_fails_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("owned");
        let err = run(
            dir.path(),
            &json!({"write_files": [{
                "path": target.to_str().unwrap(),
                "content": "x",
                "owner": "nosuchuser:root",
            }]}),
        )
        .unwrap_err();
        assert!(err.contains("Unknown user or group"), "{err}");
    }

    #[test]
    fn owner_pairs_split_the_way_upstream_splits_them() {
        assert_eq!(
            extract_usergroup("root:root"),
            (Some("root".into()), Some("root".into()))
        );
        assert_eq!(extract_usergroup("bob"), (Some("bob".into()), None));
        assert_eq!(
            extract_usergroup(" bob : staff "),
            (Some("bob".into()), Some("staff".into()))
        );
        // `-1`, `none` and empty all mean "leave this half alone".
        assert_eq!(extract_usergroup(":staff"), (None, Some("staff".into())));
        assert_eq!(extract_usergroup("bob:-1"), (Some("bob".into()), None));
        assert_eq!(extract_usergroup("None:NONE"), (None, None));
        assert_eq!(extract_usergroup(""), (None, None));
        // Only the first colon splits, so a group name may contain one.
        assert_eq!(
            extract_usergroup("bob:a:b"),
            (Some("bob".into()), Some("a:b".into()))
        );
    }

    #[test]
    fn octal_strings_parse_the_way_python_parses_them() {
        assert_eq!(parse_octal("644"), Some(0o644));
        assert_eq!(parse_octal("0644"), Some(0o644));
        assert_eq!(parse_octal("0o644"), Some(0o644));
        assert_eq!(parse_octal(" 755 "), Some(0o755));
        assert_eq!(parse_octal("6_44"), Some(0o644));
        assert_eq!(parse_octal("-1"), Some(-1));
        assert_eq!(parse_octal("648"), None);
        assert_eq!(parse_octal("abc"), None);
        assert_eq!(parse_octal(""), None);
        assert_eq!(parse_octal("_644"), None);
    }

    #[test]
    fn normpath_collapses_without_touching_the_filesystem() {
        assert_eq!(normpath("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(normpath("/a//b/./c/"), PathBuf::from("/a/b/c"));
        assert_eq!(normpath("/../../a"), PathBuf::from("/a"));
        assert_eq!(normpath("//a/b"), PathBuf::from("//a/b"));
        assert_eq!(normpath("///a/b"), PathBuf::from("/a/b"));
        assert_eq!(normpath("a/../.."), PathBuf::from(".."));
        assert_eq!(normpath("/"), PathBuf::from("/"));
    }

    #[test]
    fn base64_and_gzip_unwrap_in_the_declared_order() {
        let plain = b"hello\n";
        let gz = {
            use std::io::Write as _;
            let mut encoder = flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            );
            encoder.write_all(plain).unwrap();
            encoder.finish().unwrap()
        };
        let b64 = ci_core::b64::encode(&gz);
        assert_eq!(
            extract_contents(b64.as_bytes(), &[Extraction::Base64, Extraction::Gzip])
                .unwrap(),
            plain
        );
        assert_eq!(
            extract_contents(b"aGk=", &[Extraction::Base64]).unwrap(),
            b"hi"
        );
        assert_eq!(
            extract_contents(b"as-is", &[Extraction::Plain]).unwrap(),
            b"as-is"
        );
        // The wrong order is a decompression failure, not silent garbage.
        assert!(extract_contents(b64.as_bytes(), &[Extraction::Gzip]).is_err());
    }
}
