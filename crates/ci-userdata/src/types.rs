//! Port of `cloudinit/handlers/__init__.py`'s content-type detection.

/// `INCLUSION_TYPES_MAP` in `INCLUSION_SRCH` order: longest prefix first.
///
/// The `text/x-shellscript-per-*` entries look redundant — their prefix is the
/// content type — but upstream keeps them in the map so `devel make-mime`
/// accepts them without `--force`, and they are reachable here for a payload
/// that happens to start with one.
const INCLUSION_TYPES: &[(&str, &str)] = &[
    (
        "text/x-shellscript-per-instance",
        "text/x-shellscript-per-instance",
    ),
    ("text/x-shellscript-per-boot", "text/x-shellscript-per-boot"),
    ("text/x-shellscript-per-once", "text/x-shellscript-per-once"),
    ("#cloud-config-archive", "text/cloud-config-archive"),
    ("#cloud-config-jsonp", "text/cloud-config-jsonp"),
    ("## template: jinja", "text/jinja2"),
    ("#cloud-boothook", "text/cloud-boothook"),
    ("#include-once", "text/x-include-once-url"),
    ("#cloud-config", "text/cloud-config"),
    ("#part-handler", "text/part-handler"),
    ("#include", "text/x-include-url"),
    ("#!", "text/x-shellscript"),
];

/// Used when a message is not multipart and carries no content type.
pub const NOT_MULTIPART_TYPE: &str = "text/x-not-multipart";
/// Assigned when nothing else fits.
pub const OCTET_TYPE: &str = "application/octet-stream";
/// `UNDEF_TYPE`.
pub const UNDEF_TYPE: &str = "text/plain";
/// `ARCHIVE_UNDEF_TYPE`.
pub const ARCHIVE_UNDEF_TYPE: &str = "text/cloud-config";
/// `ARCHIVE_UNDEF_BINARY_TYPE`.
pub const ARCHIVE_UNDEF_BINARY_TYPE: &str = "application/octet-stream";

/// Types whose payload is re-examined to find the real type.
pub const TYPE_NEEDED: &[&str] = &["text/plain", "text/x-not-multipart"];
/// `INCLUDE_TYPES`.
pub const INCLUDE_TYPES: &[&str] = &["text/x-include-url", "text/x-include-once-url"];
/// `ARCHIVE_TYPES`.
pub const ARCHIVE_TYPES: &[&str] = &["text/cloud-config-archive"];

/// Content types whose payload is gzip-compressed.
pub const DECOMP_TYPES: &[&str] = &[
    "application/gzip",
    "application/gzip-compressed",
    "application/gzipped",
    "application/x-compress",
    "application/x-compressed",
    "application/x-gunzip",
    "application/x-gzip",
    "application/x-gzip-compressed",
];

/// `type_from_starts_with`: match a leading marker after stripping whitespace.
///
/// Upstream lowercases the whole payload before matching, so `#Cloud-Config`
/// is recognised.
pub fn type_from_starts_with(
    payload: &[u8],
    default: Option<&'static str>,
) -> Option<&'static str> {
    // A payload that is not UTF-8 has no type; upstream returns the default on
    // UnicodeDecodeError.
    let Ok(text) = std::str::from_utf8(payload) else {
        return default;
    };
    let lowered = text.to_lowercase();
    let trimmed = lowered.trim_start();
    INCLUSION_TYPES
        .iter()
        .find(|(prefix, _)| trimmed.starts_with(prefix))
        .map_or(default, |(_, kind)| Some(*kind))
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

    #[test]
    fn detects_the_common_markers() {
        for (payload, want) in [
            ("#cloud-config\nruncmd: []\n", "text/cloud-config"),
            ("#!/bin/sh\necho hi\n", "text/x-shellscript"),
            ("#include\nhttp://x/y\n", "text/x-include-url"),
            ("#include-once\nhttp://x/y\n", "text/x-include-once-url"),
            ("#cloud-boothook\n", "text/cloud-boothook"),
            ("#part-handler\n", "text/part-handler"),
            ("## template: jinja\n", "text/jinja2"),
            ("#cloud-config-archive\n", "text/cloud-config-archive"),
            ("#cloud-config-jsonp\n", "text/cloud-config-jsonp"),
        ] {
            assert_eq!(
                type_from_starts_with(payload.as_bytes(), None),
                Some(want),
                "{payload:?}"
            );
        }
    }

    #[test]
    fn the_longest_prefix_wins() {
        assert_eq!(
            type_from_starts_with(b"#cloud-config-archive\n", None),
            Some("text/cloud-config-archive")
        );
        assert_eq!(
            type_from_starts_with(b"#include-once\n", None),
            Some("text/x-include-once-url")
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_skips_leading_space() {
        assert_eq!(
            type_from_starts_with(b"  \n#Cloud-Config\n", None),
            Some("text/cloud-config")
        );
    }

    #[test]
    fn an_unknown_payload_gets_the_default() {
        assert_eq!(type_from_starts_with(b"hello\n", None), None);
        assert_eq!(
            type_from_starts_with(b"hello\n", Some(OCTET_TYPE)),
            Some(OCTET_TYPE)
        );
    }

    #[test]
    fn invalid_utf8_gets_the_default() {
        assert_eq!(
            type_from_starts_with(&[0xff, 0xfe, 0x00], Some(OCTET_TYPE)),
            Some(OCTET_TYPE)
        );
    }
}
