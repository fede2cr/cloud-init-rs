//! Port of `cloudinit/user_data.py`.
//!
//! Upstream builds an `email.MIMEMultipart` and hands it to the handler
//! machinery. What matters downstream is the ordered list of parts and the
//! headers each one carries, so that list is the output here rather than a
//! rebuilt message object.

use std::io::Read as _;

pub mod handlers;
pub mod mime;
pub mod types;

use mime::Message;
use types::{
    ARCHIVE_TYPES, ARCHIVE_UNDEF_BINARY_TYPE, ARCHIVE_UNDEF_TYPE, DECOMP_TYPES,
    INCLUDE_TYPES, NOT_MULTIPART_TYPE, TYPE_NEEDED, UNDEF_TYPE,
};

/// Cap on a single decompressed payload, so a gzip bomb cannot exhaust memory.
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

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
#[derive(Debug, Clone, Default)]
pub struct Processed {
    pub parts: Vec<Part>,
}

/// `_handle_error`. Upstream logs instead when `ERROR_ON_USER_DATA_FAILURE` is
/// off, but no distribution ships it off, so only the raising path is modelled.
fn handle_error(message: String) -> Result<(), Error> {
    Err(Error(message))
}

/// `UserDataProcessor.process`.
///
/// `#include` and `#include-once` parts are not fetched: the hardened metadata
/// client arrives in Phase 3, so they fail the way an unreachable URL would.
/// See docs/COMPAT.md.
pub fn process(blob: &[u8]) -> Result<Processed, Error> {
    let mut out = Processed::default();
    let msg = convert_string(blob, NOT_MULTIPART_TYPE);
    process_msg(&msg, &mut out)?;
    Ok(out)
}

/// `_process_msg`.
fn process_msg(base: &Message, out: &mut Processed) -> Result<(), Error> {
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
            match decomp_gzip(&payload) {
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

        let ctype_orig = ctype_orig.unwrap_or_else(|| UNDEF_TYPE.to_owned());
        // x-shellscript is re-examined because payloads have been seen carrying
        // user-data under that type; a real shell script always has a `#!`.
        let ctype = if TYPE_NEEDED.contains(&ctype_orig.as_str())
            || ctype_orig == "text/x-shellscript"
        {
            type_or(&payload, &ctype_orig)
        } else {
            ctype_orig
        };

        if INCLUDE_TYPES.contains(&ctype.as_str()) {
            record_includes(&payload)?;
            continue;
        }
        if ARCHIVE_TYPES.contains(&ctype.as_str()) {
            explode_archive(&payload, out);
            continue;
        }
        // The payload here is the decompressed one, which is what upstream
        // copies onto the replacement part before reading its launch-index.
        let index = launch_index(part.header("Launch-Index"), &ctype, &payload);
        attach(out, &ctype, part.filename(), payload, index);
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
    content_type: &str,
    filename: Option<String>,
    payload: Vec<u8>,
    launch_index: Option<i64>,
) {
    let index = out.parts.len() + 1;
    out.parts.push(Part {
        content_type: content_type.to_owned(),
        filename: filename.unwrap_or_else(|| part_filename(index)),
        payload,
        launch_index,
    });
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
        let (content, declared_type, filename, entry_index) =
            if let Some(text) = entry.as_str() {
                (text.as_bytes().to_vec(), None, None, None)
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
        attach(out, &mtype, filename, content, index);
    }
}

/// `_do_include` without the fetching: record each URL so the caller can see
/// what was skipped.
fn record_includes(content: &[u8]) -> Result<(), Error> {
    let Ok(text) = std::str::from_utf8(content) else {
        return Ok(());
    };
    for line in text.lines() {
        let lowered = line.to_lowercase();
        let stripped = if lowered.starts_with("#include-once") {
            line.get("#include-once".len()..).unwrap_or("").trim_start()
        } else if lowered.starts_with("#include") {
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
        handle_error(format!(
            "Fetching from {url} resulted in: #include is not implemented in this port"
        ))?;
    }
    Ok(())
}

/// `convert_string`: gunzip, then either parse as MIME or wrap as one part.
fn convert_string(raw: &[u8], content_type: &str) -> Message {
    // decomp_gzip(quiet=True): non-gzip data passes through unchanged.
    let data = decomp_gzip(raw).unwrap_or_else(|_| raw.to_vec());
    let head = data.get(..data.len().min(4096)).unwrap_or(&data);
    if contains_ignore_ascii_case(head, b"mime-version:") {
        mime::parse(&data)
    } else {
        Message::leaf(content_type, data)
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

/// `decomp_gzip`.
fn decomp_gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(MAX_DECOMPRESSED)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
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
    fn an_include_is_an_error_because_fetching_is_not_implemented() {
        let err = process(b"#include\nhttp://example.invalid/a\n").unwrap_err();
        assert!(err.0.contains("http://example.invalid/a"), "{err}");
    }

    #[test]
    fn comments_and_blank_lines_in_an_include_are_skipped() {
        // Only the one real URL is reached, so the error names it.
        let err = process(b"#include\n# a comment\n\nhttp://example.invalid/a\n")
            .unwrap_err();
        assert!(err.0.contains("http://example.invalid/a"), "{err}");
    }

    #[test]
    fn an_include_with_no_urls_yields_nothing() {
        let out = process(b"#include\n# only a comment\n").unwrap();
        assert!(out.parts.is_empty());
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
