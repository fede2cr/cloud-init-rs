//! Port of `cloudinit/user_data.py`.
//!
//! Upstream builds an `email.MIMEMultipart` and hands it to the handler
//! machinery. What matters downstream is the ordered list of parts and the
//! headers each one carries, so that list is the output here rather than a
//! rebuilt message object.

use std::path::PathBuf;

pub mod handlers;
pub mod mime;
pub mod types;

use ci_core::Paths;
use mime::Message;
use types::{
    ARCHIVE_TYPES, ARCHIVE_UNDEF_BINARY_TYPE, ARCHIVE_UNDEF_TYPE, DECOMP_TYPES,
    INCLUDE_TYPES, NOT_MULTIPART_TYPE, TYPE_NEEDED, UNDEF_TYPE,
};

/// How deep a chain of `#include` documents may go.
///
/// Upstream has no limit: an include that names itself recurses until the
/// interpreter's stack runs out. See docs/COMPAT.md.
const MAX_INCLUDE_DEPTH: usize = 16;

/// `PART_FN_TPL`.
fn part_filename(index: usize) -> String {
    format!("part-{index:03}")
}

/// One attachment in the processed user-data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The resolved `Content-Type`.
    pub content_type: String,
    /// `Content-Disposition` filename, always set once attached.
    pub filename: String,
    /// The decoded, decompressed body.
    pub payload: Vec<u8>,
    /// `Launch-Index`, when the part or its payload declared one.
    pub launch_index: Option<i64>,
    /// `Merge-Type`, or `X-Merge-Type` when that is absent. Only the
    /// cloud-config handler reads it.
    pub merge_type: Option<String>,
}

impl Part {
    /// The payload as text, when it is valid UTF-8.
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.payload).ok()
    }
}

/// A part that could not be processed. `_handle_error`'s `RuntimeError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The result of walking one user-data blob.
#[derive(Debug, Clone)]
pub struct Processed {
    pub parts: Vec<Part>,
    /// The `MIMEMultipart` upstream accumulates into. The handlers only need
    /// [`Processed::parts`], but `Init.update` writes this message back out.
    pub message: Message,
}

impl Default for Processed {
    fn default() -> Self {
        Self {
            parts: Vec::new(),
            message: Message::multipart(&boundary()),
        }
    }
}

/// `email.generator._make_boundary`: fifteen `=`, a zero-padded 19-digit
/// number below `sys.maxsize`, then two more `=`.
fn boundary() -> String {
    let token = ci_sys::rand::u64() >> 1;
    format!("==============={token:019}==")
}

/// `_handle_error`. Upstream logs instead when `ERROR_ON_USER_DATA_FAILURE` is
/// off, but no distribution ships it off, so only the raising path is modelled.
fn handle_error(message: String) -> Result<(), Error> {
    Err(Error(message))
}

/// `UserDataProcessor.process`.
pub fn process(blob: &[u8], paths: &Paths) -> Result<Processed, Error> {
    let mut out = Processed::default();
    let env = Env {
        cache_dir: paths.instance_path(ci_core::Lookup::Data).join("urlcache"),
        fetch: ci_url::Config {
            // `UserDataProcessor.__init__`: an `#include` over https offers
            // the instance's client certificate if one has been dropped in.
            ssl: ci_url::fetch_ssl_details(paths),
            ..ci_url::Config::default()
        },
    };
    let msg = convert_string(blob, NOT_MULTIPART_TYPE);
    process_msg(&msg, &mut out, &env, 0)?;
    Ok(out)
}

/// What the walk needs beyond the message itself: where `#include-once` caches
/// its answers, and how the fetches are bounded.
#[derive(Debug, Clone)]
struct Env {
    cache_dir: PathBuf,
    fetch: ci_url::Config,
}

/// `_process_msg`.
fn process_msg(
    base: &Message,
    out: &mut Processed,
    env: &Env,
    depth: usize,
) -> Result<(), Error> {
    for part in base.walk() {
        if part.content_maintype() == "multipart" {
            continue;
        }
        let mut ctype_orig = Some(part.content_type());
        let mut payload = part.decoded_payload();

        if ctype_orig
            .as_deref()
            .is_some_and(|c| DECOMP_TYPES.contains(&c))
        {
            match ci_core::gzip::decompress(&payload) {
                Ok(decompressed) => {
                    payload = decompressed;
                    ctype_orig = None;
                }
                Err(e) => {
                    handle_error(format!(
                        "Failed decompressing payload from {} of length {} due to: {e}",
                        part.content_type(),
                        payload.len()
                    ))?;
                    continue;
                }
            }
        }

        let was_compressed = ctype_orig.is_none();
        let ctype_orig = ctype_orig.unwrap_or_else(|| UNDEF_TYPE.to_owned());
        // x-shellscript is re-examined because payloads have been seen carrying
        // user-data under that type; a real shell script always has a `#!`.
        let ctype = if TYPE_NEEDED.contains(&ctype_orig.as_str())
            || ctype_orig == "text/x-shellscript"
        {
            type_or(&payload, &ctype_orig)
        } else {
            ctype_orig.clone()
        };

        if INCLUDE_TYPES.contains(&ctype.as_str()) {
            do_include(&payload, out, env, depth)?;
            continue;
        }
        if ARCHIVE_TYPES.contains(&ctype.as_str()) {
            explode_archive(&payload, out);
            continue;
        }
        // The payload here is the decompressed one, which is what upstream
        // copies onto the replacement part before reading its launch-index.
        let index = launch_index(part.header("Launch-Index"), &ctype, &payload);
        let merge = part
            .header_exact("Merge-Type")
            .or_else(|| part.header_exact("X-Merge-Type"))
            .map(str::to_owned);
        // A decompressed part is rebuilt from scratch, so only the two headers
        // that still mean anything are carried over.
        let mut message = if was_compressed {
            let mut fresh = Message::non_multipart(&ctype, payload.clone());
            if let Some(name) = part.filename() {
                fresh.set_filename(&name);
            }
            if let Some(value) = part.header("Launch-Index") {
                fresh.set_header("Launch-Index", value);
            }
            fresh
        } else {
            part.clone()
        };
        if ctype != ctype_orig {
            message.set_header("Content-Type", &ctype);
        }
        // Upstream then replaces the header on `base_msg` as well, which its
        // own TODO admits is probably wrong. When the blob was not multipart
        // the base *is* this part, so the replacement moves `Content-Type`
        // below `MIME-Version` and that ordering reaches the written file.
        if !was_compressed && std::ptr::eq(part, base) {
            message.set_header("Content-Type", &ctype);
        }
        attach(out, message, &ctype, part.filename(), payload, index, merge);
    }
    Ok(())
}

fn type_or(payload: &[u8], fallback: &str) -> String {
    types::type_from_starts_with(payload, None)
        .map_or_else(|| fallback.to_owned(), str::to_owned)
}

/// `_attach_part` plus `_process_before_attach`.
fn attach(
    out: &mut Processed,
    mut message: Message,
    content_type: &str,
    filename: Option<String>,
    payload: Vec<u8>,
    launch_index: Option<i64>,
    merge_type: Option<String>,
) {
    let index = out.parts.len() + 1;
    let filename = filename.unwrap_or_else(|| {
        let generated = part_filename(index);
        message.set_filename(&generated);
        generated
    });
    // `_attach_launch_index` appends, so a part that already declared one
    // carries the header twice.
    if let Some(value) = launch_index {
        message.add_header("Launch-Index", &value.to_string());
    }
    out.parts.push(Part {
        content_type: content_type.to_owned(),
        filename,
        payload,
        launch_index,
        merge_type,
    });
    out.message.attach(message);
    out.message
        .set_header("Number-Attachments", &index.to_string());
}

/// `_attach_launch_index`: the header wins over a `launch-index` in the body.
fn launch_index(header: Option<&str>, ctype: &str, payload: &[u8]) -> Option<i64> {
    let header = header.and_then(|v| v.trim().parse::<i64>().ok());
    if header.is_some() {
        return header;
    }
    // Only cloud-config payloads are examined; EXAMINE_FOR_LAUNCH_INDEX.
    if ctype != "text/cloud-config" {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    let cfg = ci_config::load_yaml(text, ci_config::Limits::default()).ok()?;
    cfg.get("launch-index").and_then(value_as_index)
}

fn value_as_index(value: &ci_config::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// `_explode_archive`: a `#cloud-config-archive` is a YAML list of entries.
fn explode_archive(archive: &[u8], out: &mut Processed) {
    // `load_yaml(archive, default=[], allowed=(list, set))`: anything that is
    // not parseable, or not a list, yields the default and attaches nothing.
    let Ok(text) = std::str::from_utf8(archive) else {
        return;
    };
    let Ok(value) = ci_config::load_yaml(text, ci_config::Limits::default()) else {
        return;
    };
    let Some(entries) = value.as_array() else {
        return;
    };

    for entry in entries {
        let empty = ci_config::Object::new();
        let (content, declared_type, filename, entry_index, merge, map) =
            if let Some(text) = entry.as_str() {
                (text.as_bytes().to_vec(), None, None, None, None, &empty)
            } else if let Some(map) = entry.as_object() {
                let content = map
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                (
                    content,
                    map.get("type").and_then(|t| t.as_str()).map(str::to_owned),
                    map.get("filename")
                        .and_then(|f| f.as_str())
                        .map(str::to_owned),
                    map.get("launch-index").and_then(value_as_index),
                    entry_merge_type(map),
                    map,
                )
            } else {
                continue;
            };

        let mtype = declared_type.unwrap_or_else(|| {
            // The port's YAML values are always text, so the binary default
            // cannot be reached; see docs/COMPAT.md.
            let default = if std::str::from_utf8(&content).is_ok() {
                ARCHIVE_UNDEF_TYPE
            } else {
                ARCHIVE_UNDEF_BINARY_TYPE
            };
            type_or(&content, default)
        });
        // Every attached part gets the launch-index treatment, so an archive
        // entry with no explicit key still has its payload examined.
        let index = entry_index.or_else(|| launch_index(None, &mtype, &content));
        let message = archive_message(&mtype, &content, map, filename.as_deref());
        attach(out, message, &mtype, filename, content, index, merge);
    }
}

/// The part `_explode_archive` builds: `MIMEText` for a text entry, which
/// carries a charset and a transfer encoding, `MIMEBase` for anything else.
fn archive_message(
    mtype: &str,
    content: &[u8],
    entry: &ci_config::Object,
    filename: Option<&str>,
) -> Message {
    let mut message = if mtype.starts_with("text/") {
        let ascii = content.is_ascii();
        let charset = if ascii { "us-ascii" } else { "utf-8" };
        let body = if ascii {
            content.to_vec()
        } else {
            base64_body(content).into_bytes()
        };
        let mut message =
            Message::non_multipart(&format!("{mtype}; charset=\"{charset}\""), body);
        message.add_header(
            "Content-Transfer-Encoding",
            if ascii { "7bit" } else { "base64" },
        );
        message
    } else {
        Message::non_multipart(mtype, content.to_vec())
    };
    if let Some(filename) = filename {
        message.set_filename(filename);
    }
    if let Some(value) = entry.get("launch-index") {
        message.add_header("Launch-Index", &python_str(value));
    }
    for (key, value) in entry {
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "content"
                | "filename"
                | "type"
                | "launch-index"
                | "content-disposition"
                | "number-attachments"
                | "content-type"
        ) {
            continue;
        }
        message.add_header(key, &python_str(value));
    }
    message
}

/// `email.base64mime.body_encode`: 76 columns per line, every line terminated.
fn base64_body(content: &[u8]) -> String {
    let mut out = String::new();
    for chunk in content.chunks(57) {
        out.push_str(&ci_core::b64::encode(chunk));
        out.push('\n');
    }
    out
}

/// Upstream hands the raw YAML value to `add_header`, which only accepts a
/// string; the port renders it instead (COMPAT.md B34).
fn python_str(value: &ci_config::Value) -> String {
    match value {
        ci_config::Value::String(text) => text.clone(),
        ci_config::Value::Bool(true) => "True".to_owned(),
        ci_config::Value::Bool(false) => "False".to_owned(),
        ci_config::Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

/// Upstream copies every entry key it does not consume onto the part as a
/// header, and the handler then reads that header out of a plain dictionary,
/// so the key has to be spelled exactly `Merge-Type` or `X-Merge-Type`.
fn entry_merge_type(entry: &ci_config::Object) -> Option<String> {
    let find = |name: &str| {
        entry
            .get(name)
            .and_then(ci_config::Value::as_str)
            .map(str::to_owned)
    };
    find("Merge-Type").or_else(|| find("X-Merge-Type"))
}

/// `_do_include`: each line is a URL, and the fetched document is walked in
/// place of the include part.
fn do_include(
    content: &[u8],
    out: &mut Processed,
    env: &Env,
    depth: usize,
) -> Result<(), Error> {
    // `fully_decoded_payload` decodes a text part with surrogate escapes rather
    // than dropping it, so one bad byte must not take the valid lines with it.
    let text = String::from_utf8_lossy(content);
    if depth >= MAX_INCLUDE_DEPTH {
        return handle_error(format!(
            "Fetching from an #include nested more than {MAX_INCLUDE_DEPTH} deep"
        ));
    }
    let mut include_once = false;
    for line in split_lines(&text) {
        let lowered = line.to_lowercase();
        let stripped = if lowered.starts_with("#include-once") {
            include_once = true;
            line.get("#include-once".len()..).unwrap_or("").trim_start()
        } else if lowered.starts_with("#include") {
            include_once = false;
            line.get("#include".len()..).unwrap_or("").trim_start()
        } else {
            line
        };
        if stripped.starts_with('#') {
            continue;
        }
        let url = stripped.trim();
        if url.is_empty() {
            continue;
        }

        let cached = include_once
            .then(|| env.cache_dir.join(ci_core::hash::md5_hex(url.as_bytes())));
        let fetched = match cached.as_ref().filter(|path| path.is_file()) {
            Some(path) => std::fs::read(path).ok(),
            None => fetch(url, cached.as_deref(), env)?,
        };
        if let Some(fetched) = fetched {
            let msg = convert_string(&fetched, NOT_MULTIPART_TYPE);
            process_msg(&msg, out, env, depth + 1)?;
        }
    }
    Ok(())
}

/// Python's `str.splitlines`, which breaks on more than `\n`. An include list
/// separated by `\r` or `\x0b` is two URLs upstream, so it is two here.
use ci_core::pystr::split_lines;

/// One include URL. `Ok(None)` means the failure was recorded and the walk goes
/// on, which only happens when `ERROR_ON_USER_DATA_FAILURE` is off upstream.
fn fetch(
    url: &str,
    cache: Option<&std::path::Path>,
    env: &Env,
) -> Result<Option<Vec<u8>>, Error> {
    match ci_url::read_file_or_url(url, &env.fetch) {
        Ok(response) if response.ok() => {
            if let Some(cache) = cache {
                // A failed write is upstream's `write_file`, which raises; the
                // include itself succeeded, so nothing is retried here.
                let _ = std::fs::create_dir_all(cache.parent().unwrap_or(cache));
                let _ = ci_sys::atomic::write_file(
                    cache,
                    &response.contents,
                    ci_sys::atomic::WriteOptions::SECRET,
                );
            }
            Ok(Some(response.contents))
        }
        Ok(response) => handle_error(format!(
            "Fetching from {url} resulted in a invalid http code of {}",
            response.code
        ))
        .map(|()| None),
        Err(e) => handle_error(e.to_string()).map(|()| None),
    }
}

/// `convert_string`: gunzip, then either parse as MIME or wrap as one part.
fn convert_string(raw: &[u8], content_type: &str) -> Message {
    // decomp_gzip(quiet=True): non-gzip data passes through unchanged.
    let data = ci_core::gzip::decompress(raw).unwrap_or_else(|_| raw.to_vec());
    let head = data.get(..data.len().min(4096)).unwrap_or(&data);
    if contains_ignore_ascii_case(head, b"mime-version:") {
        mime::parse(&data)
    } else {
        Message::non_multipart(content_type, data)
    }
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
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

    /// Most cases never reach an include, so they need no state directory.
    fn process(blob: &[u8]) -> Result<Processed, Error> {
        super::process(blob, &Paths::default())
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_bare_cloud_config_becomes_one_part() {
        let out = process(b"#cloud-config\nruncmd: []\n").unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
        assert_eq!(out.parts[0].filename, "part-001");
        assert_eq!(out.parts[0].text().unwrap(), "#cloud-config\nruncmd: []\n");
    }

    #[test]
    fn a_bare_shell_script_is_detected() {
        let out = process(b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(out.parts[0].content_type, "text/x-shellscript");
    }

    #[test]
    fn unrecognised_user_data_keeps_the_not_multipart_type() {
        let out = process(b"just some text\n").unwrap();
        assert_eq!(out.parts[0].content_type, NOT_MULTIPART_TYPE);
    }

    #[test]
    fn gzipped_user_data_is_decompressed_before_detection() {
        let out = process(&gzip(b"#cloud-config\nruncmd: []\n")).unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
    }

    #[test]
    fn a_multipart_message_yields_a_part_each() {
        let raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: text/cloud-config\n\n#cloud-config\n\
--B\nContent-Type: text/x-shellscript\n\n#!/bin/sh\n\
--B--\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts.len(), 2);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
        assert_eq!(out.parts[0].filename, "part-001");
        assert_eq!(out.parts[1].content_type, "text/x-shellscript");
        assert_eq!(out.parts[1].filename, "part-002");
    }

    #[test]
    fn a_declared_shellscript_that_is_really_cloud_config_is_reclassified() {
        let raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: text/x-shellscript\n\n#cloud-config\nruncmd: []\n\
--B--\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
    }

    #[test]
    fn an_existing_filename_is_kept() {
        let raw = b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: text/cloud-config\nContent-Disposition: attachment; filename=\"mine.yaml\"\n\n#cloud-config\n\
--B--\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].filename, "mine.yaml");
    }

    #[test]
    fn an_archive_explodes_into_its_entries() {
        let raw = b"#cloud-config-archive\n\
- type: text/cloud-config\n  content: |\n    #cloud-config\n    runcmd: []\n\
- filename: run.sh\n  content: |\n    #!/bin/sh\n    echo hi\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts.len(), 2);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
        assert_eq!(out.parts[1].content_type, "text/x-shellscript");
        assert_eq!(out.parts[1].filename, "run.sh");
    }

    #[test]
    fn a_bare_string_archive_entry_is_typed_by_its_content() {
        let raw = b"#cloud-config-archive\n- |\n  #!/bin/sh\n  echo hi\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].content_type, "text/x-shellscript");
    }

    #[test]
    fn an_archive_entry_without_a_marker_defaults_to_cloud_config() {
        let raw = b"#cloud-config-archive\n- |\n  runcmd: []\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].content_type, ARCHIVE_UNDEF_TYPE);
    }

    #[test]
    fn a_launch_index_header_wins_over_the_payload() {
        let raw = b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: text/cloud-config\nLaunch-Index: 5\n\n#cloud-config\nlaunch-index: 2\n\
--B--\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].launch_index, Some(5));
    }

    #[test]
    fn a_launch_index_in_a_cloud_config_payload_is_read() {
        let out = process(b"#cloud-config\nlaunch-index: 3\n").unwrap();
        assert_eq!(out.parts[0].launch_index, Some(3));
    }

    #[test]
    fn an_unreachable_include_is_fatal() {
        let err = process(b"#include\nfile:///nonexistent/seed\n").unwrap_err();
        assert!(err.0.contains("/nonexistent/seed"), "{err}");
    }

    #[test]
    fn comments_and_blank_lines_in_an_include_are_skipped() {
        // Only the one real URL is reached, so the error names it.
        let err = process(b"#include\n# a comment\n\nfile:///nonexistent/seed\n")
            .unwrap_err();
        assert!(err.0.contains("/nonexistent/seed"), "{err}");
    }

    #[test]
    fn an_include_with_no_urls_yields_nothing() {
        let out = process(b"#include\n# only a comment\n").unwrap();
        assert!(out.parts.is_empty());
    }

    #[test]
    fn an_include_list_breaks_on_every_python_line_boundary() {
        for sep in [
            "\r", "\r\n", "\u{b}", "\u{c}", "\u{1c}", "\u{85}", "\u{2028}",
        ] {
            let blob = format!("#include\nfile:///nonexistent/a{sep}second");
            assert_eq!(
                super::split_lines(&blob).len(),
                3,
                "{sep:?} did not break the list"
            );
        }
    }

    #[test]
    fn an_uppercase_marker_still_turns_include_once_on() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().to_path_buf(),
            ..Paths::default()
        };
        let seed = dir.path().join("seed");
        std::fs::write(&seed, b"#cloud-config\nruncmd: []\n").unwrap();
        let blob = format!("#include\n#INCLUDE-ONCE {}\n", seed.display());

        super::process(blob.as_bytes(), &paths).unwrap();
        assert!(paths
            .instance_path(ci_core::Lookup::Data)
            .join("urlcache")
            .join(ci_core::hash::md5_hex(
                seed.display().to_string().as_bytes()
            ))
            .is_file());
    }

    #[test]
    fn an_included_document_is_walked_in_place_of_the_include() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::write(&seed, b"#cloud-config\nruncmd: []\n").unwrap();
        let blob = format!("#include\n{}\n", seed.display());

        let out = process(blob.as_bytes()).unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
        assert_eq!(out.parts[0].filename, "part-001");
    }

    #[test]
    fn include_once_caches_the_answer_and_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            cloud_dir: dir.path().to_path_buf(),
            ..Paths::default()
        };
        let seed = dir.path().join("seed");
        std::fs::write(&seed, b"#cloud-config\nruncmd: []\n").unwrap();
        let blob = format!("#include-once\n{}\n", seed.display());

        super::process(blob.as_bytes(), &paths).unwrap();
        let cached = paths
            .instance_path(ci_core::Lookup::Data)
            .join("urlcache")
            .join(ci_core::hash::md5_hex(
                seed.display().to_string().as_bytes(),
            ));
        assert_eq!(
            std::fs::read(&cached).unwrap(),
            b"#cloud-config\nruncmd: []\n"
        );

        // The cache, not the file, is what a second walk reads.
        std::fs::write(&cached, b"#cloud-config\nruncmd: [second]\n").unwrap();
        let out = super::process(blob.as_bytes(), &paths).unwrap();
        assert_eq!(
            out.parts[0].text().unwrap(),
            "#cloud-config\nruncmd: [second]\n"
        );
    }

    #[test]
    fn an_include_that_names_itself_stops_at_the_depth_cap() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        std::fs::write(&seed, format!("#include\n{}\n", seed.display())).unwrap();

        let err =
            process(format!("#include\n{}\n", seed.display()).as_bytes()).unwrap_err();
        assert!(err.0.contains("nested more than"), "{err}");
    }

    #[test]
    fn a_gzip_part_inside_a_message_is_decompressed() {
        let mut raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: application/x-gzip\nContent-Transfer-Encoding: base64\n\n"
                .to_vec();
        let compressed = gzip(b"#cloud-config\nruncmd: []\n");
        raw.extend(base64_encode(&compressed).as_bytes());
        raw.extend(b"\n--B--\n");
        let out = process(&raw).unwrap();
        assert_eq!(out.parts.len(), 1);
        assert_eq!(out.parts[0].content_type, "text/cloud-config");
    }

    #[test]
    fn a_corrupt_gzip_part_is_fatal() {
        // features.ERROR_ON_USER_DATA_FAILURE: upstream raises rather than logs.
        let raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: application/x-gzip\n\nnot actually gzip\n--B--\n";
        let err = process(raw).unwrap_err();
        assert!(err.0.starts_with("Failed decompressing payload"), "{err}");
    }

    #[test]
    fn a_merge_type_header_is_carried_on_the_part() {
        let raw = b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\n\n\
--B\nContent-Type: text/cloud-config\nX-Merge-Type: dict(no_replace)\n\nfoo: 1\n\
--B\nContent-Type: text/cloud-config\nMerge-Type: dict(recurse_dict)\nX-Merge-Type: dict(no_replace)\n\nfoo: 2\n\
--B\nContent-Type: text/cloud-config\nmerge-type: dict(recurse_dict)\n\nfoo: 3\n--B--\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].merge_type.as_deref(), Some("dict(no_replace)"));
        assert_eq!(
            out.parts[1].merge_type.as_deref(),
            Some("dict(recurse_dict)")
        );
        // The handler looks its headers up in a plain dict, so a header in any
        // other case is never seen; see docs/COMPAT.md.
        assert_eq!(out.parts[2].merge_type, None);
    }

    #[test]
    fn an_archive_entry_carries_its_merge_type_key() {
        let raw = b"#cloud-config-archive\n\
- {type: text/cloud-config, Merge-Type: 'dict(recurse_dict)', content: 'foo: 1'}\n\
- {type: text/cloud-config, merge-type: 'dict(recurse_dict)', content: 'foo: 2'}\n";
        let out = process(raw).unwrap();
        assert_eq!(
            out.parts[0].merge_type.as_deref(),
            Some("dict(recurse_dict)")
        );
        assert_eq!(out.parts[1].merge_type, None);
    }

    #[test]
    fn a_launch_index_is_read_from_the_decompressed_payload() {
        let mut raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"B\"\nContent-Transfer-Encoding: base64\n\n--B\nContent-Type: application/x-gzip\nContent-Transfer-Encoding: base64\n\n"
                .to_vec();
        raw.extend(
            base64_encode(&gzip(b"#cloud-config\nlaunch-index: 4\n")).as_bytes(),
        );
        raw.extend(b"\n--B--\n");
        let out = process(&raw).unwrap();
        assert_eq!(out.parts[0].launch_index, Some(4));
    }

    #[test]
    fn an_archive_entry_gets_its_launch_index_from_its_payload() {
        let raw = b"#cloud-config-archive\n- content: \"#cloud-config\\nlaunch-index: 9\\n\"\n";
        let out = process(raw).unwrap();
        assert_eq!(out.parts[0].launch_index, Some(9));
    }

    fn base64_encode(data: &[u8]) -> String {
        const ALPHABET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
            let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
            let n = b0 << 16 | b1 << 8 | b2;
            out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6 & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }
}
