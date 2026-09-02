//! A MIME reader covering what `email.message_from_string` gives cloud-init.
//!
//! Only the parts of RFC 2045/2046 that user-data actually uses are modelled:
//! headers with folded continuations, `multipart/*` boundaries, and the
//! `base64`/`quoted-printable` content transfer encodings. Nesting is bounded
//! so a crafted payload cannot drive unbounded recursion.

use std::fmt;

/// How deep `multipart/*` nesting may go before parts are left undecoded.
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone)]
enum Body {
    Leaf(Vec<u8>),
    Multipart(Vec<Message>),
}

/// One MIME entity.
#[derive(Debug, Clone)]
pub struct Message {
    headers: Vec<(String, String)>,
    body: Body,
}

impl Message {
    /// A message with no headers wrapping `payload`.
    pub fn leaf(content_type: &str, payload: Vec<u8>) -> Self {
        Self {
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body: Body::Leaf(payload),
        }
    }

    /// First value for `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.to_owned()));
    }

    /// `get_content_type()`, lowercased, defaulting to `text/plain`.
    pub fn content_type(&self) -> String {
        let raw = self.header("Content-Type").unwrap_or("text/plain");
        let value = raw.split(';').next().unwrap_or("text/plain").trim();
        if value.contains('/') {
            value.to_ascii_lowercase()
        } else {
            "text/plain".to_owned()
        }
    }

    pub fn content_maintype(&self) -> String {
        let ctype = self.content_type();
        ctype
            .split_once('/')
            .map_or(ctype.clone(), |(main, _)| main.to_owned())
    }

    /// `get_filename()` from `Content-Disposition`, falling back to the
    /// `name` parameter of `Content-Type` as Python does.
    pub fn filename(&self) -> Option<String> {
        self.header("Content-Disposition")
            .and_then(|v| param(v, "filename"))
            .or_else(|| self.header("Content-Type").and_then(|v| param(v, "name")))
    }

    pub fn set_filename(&mut self, filename: &str) {
        self.set_header(
            "Content-Disposition",
            &format!("attachment; filename=\"{}\"", escape_param(filename)),
        );
    }

    pub fn is_multipart(&self) -> bool {
        matches!(self.body, Body::Multipart(_))
    }

    /// `get_payload(decode=True)`: the body after undoing the transfer encoding.
    pub fn decoded_payload(&self) -> Vec<u8> {
        let Body::Leaf(raw) = &self.body else {
            return Vec::new();
        };
        match self
            .header("Content-Transfer-Encoding")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("base64") => decode_base64(raw),
            Some("quoted-printable") => decode_quoted_printable(raw),
            _ => raw.clone(),
        }
    }

    pub fn set_payload(&mut self, payload: Vec<u8>) {
        self.body = Body::Leaf(payload);
    }

    pub fn parts(&self) -> &[Message] {
        match &self.body {
            Body::Multipart(parts) => parts,
            Body::Leaf(_) => &[],
        }
    }

    pub fn attach(&mut self, part: Message) {
        match &mut self.body {
            Body::Multipart(parts) => parts.push(part),
            Body::Leaf(_) => self.body = Body::Multipart(vec![part]),
        }
    }

    /// `walk()`: this message, then every subpart, depth first.
    pub fn walk(&self) -> Vec<&Message> {
        let mut out = vec![self];
        for part in self.parts() {
            out.extend(part.walk());
        }
        out
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} message>", self.content_type())
    }
}

/// Parse a complete MIME document.
pub fn parse(raw: &[u8]) -> Message {
    parse_at(raw, 0)
}

fn parse_at(raw: &[u8], depth: usize) -> Message {
    let (headers, body) = split_headers(raw);
    let mut msg = Message {
        headers,
        body: Body::Leaf(body.to_vec()),
    };
    if msg.content_maintype() != "multipart" || depth >= MAX_DEPTH {
        return msg;
    }
    let Some(boundary) = msg
        .header("Content-Type")
        .and_then(|v| param(v, "boundary"))
    else {
        return msg;
    };
    let parts = split_parts(body, &boundary)
        .into_iter()
        .map(|part| parse_at(part, depth + 1))
        .collect();
    msg.body = Body::Multipart(parts);
    msg
}

/// Headers end at the first blank line; a body without one is all headers.
fn split_headers(raw: &[u8]) -> (Vec<(String, String)>, &[u8]) {
    let mut headers = Vec::new();
    let mut rest = raw;
    let mut current: Option<(String, String)> = None;

    loop {
        let (line, tail) = take_line(rest);
        let trimmed = trim_eol(line);
        if trimmed.is_empty() {
            rest = tail;
            break;
        }
        let Ok(text) = std::str::from_utf8(trimmed) else {
            // A non-UTF-8 header line means this is not really a header block.
            break;
        };
        if text.starts_with(' ') || text.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(text.trim());
            }
        } else if let Some((name, value)) = text.split_once(':') {
            if let Some(done) = current.take() {
                headers.push(done);
            }
            current = Some((name.trim().to_owned(), value.trim().to_owned()));
        } else {
            break;
        }
        rest = tail;
        if tail.is_empty() {
            break;
        }
    }
    if let Some(done) = current.take() {
        headers.push(done);
    }
    (headers, rest)
}

fn take_line(raw: &[u8]) -> (&[u8], &[u8]) {
    match raw.iter().position(|b| *b == b'\n') {
        Some(idx) => raw.split_at(idx + 1),
        None => (raw, &[]),
    }
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line.get(end - 1), Some(b'\n' | b'\r')) {
        end -= 1;
    }
    line.get(..end).unwrap_or(&[])
}

/// Split a multipart body on `--boundary`, dropping the preamble and epilogue.
fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut current: Option<usize> = None;
    let mut rest = body;
    let mut offset = 0usize;

    while !rest.is_empty() {
        let (line, tail) = take_line(rest);
        let consumed = line.len();
        let trimmed = trim_eol(line);
        if trimmed.starts_with(delimiter.as_bytes()) {
            let is_close = trimmed
                .get(delimiter.len()..)
                .is_some_and(|t| t.starts_with(b"--"));
            if let Some(start) = current.take() {
                if let Some(part) = body.get(start..offset) {
                    parts.push(strip_trailing_eol(part));
                }
            }
            if is_close {
                return parts;
            }
            current = Some(offset + consumed);
        }
        offset += consumed;
        rest = tail;
    }
    if let Some(start) = current {
        if let Some(part) = body.get(start..) {
            parts.push(strip_trailing_eol(part));
        }
    }
    parts
}

/// The CRLF before a boundary belongs to the delimiter, not the part.
fn strip_trailing_eol(part: &[u8]) -> &[u8] {
    let mut end = part.len();
    if end > 0 && part.get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    if end > 0 && part.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    part.get(..end).unwrap_or(&[])
}

/// A `name=value` parameter from a structured header, quotes removed.
fn param(header: &str, name: &str) -> Option<String> {
    let mut plain = None;
    // RFC 2231 segments, keyed by their index, with a flag for percent-encoding.
    let mut segments: Vec<(u32, bool, String)> = Vec::new();

    for piece in header.split(';').skip(1) {
        let Some((key, value)) = piece.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());

        if key.eq_ignore_ascii_case(name) {
            plain.get_or_insert(value);
            continue;
        }
        let Some(rest) = strip_prefix_ignore_case(key, name) else {
            continue;
        };
        // `filename*`, `filename*0`, `filename*0*`: anything else is a
        // different parameter that merely shares a prefix.
        let Some(rest) = rest.strip_prefix('*') else {
            continue;
        };
        let (digits, encoded) = match rest.strip_suffix('*') {
            Some(digits) => (digits, true),
            None => (rest, false),
        };
        if digits.is_empty() {
            // Bare `filename*` is always the extended form.
            segments.push((0, true, value));
        } else if let Ok(index) = digits.parse::<u32>() {
            segments.push((index, encoded, value));
        }
    }

    if segments.is_empty() {
        return plain;
    }
    segments.sort_by_key(|(index, _, _)| *index);
    Some(collapse_rfc2231(&segments))
}

/// `email.utils.collapse_rfc2231_value`: join the segments, percent-decode the
/// ones that are encoded, and read the charset off the first one.
fn collapse_rfc2231(segments: &[(u32, bool, String)]) -> String {
    let mut charset = String::new();
    let mut bytes = Vec::new();

    for (position, (_, encoded, value)) in segments.iter().enumerate() {
        if !encoded {
            bytes.extend_from_slice(value.as_bytes());
            continue;
        }
        // Only the first segment carries `charset'language'`.
        let text = if position == 0 {
            match value.split_once('\'').and_then(|(cs, rest)| {
                rest.split_once('\'').map(|(_lang, tail)| (cs, tail))
            }) {
                Some((cs, tail)) => {
                    charset = cs.to_ascii_lowercase();
                    tail
                }
                None => value.as_str(),
            }
        } else {
            value.as_str()
        };
        bytes.extend(percent_decode(text));
    }

    if charset == "iso-8859-1" || charset == "latin-1" {
        return bytes.iter().map(|b| char::from(*b)).collect();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn percent_decode(text: &str) -> Vec<u8> {
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut idx = 0;
    while let Some(byte) = raw.get(idx) {
        if *byte == b'%' {
            if let (Some(hi), Some(lo)) = (
                raw.get(idx + 1).copied().and_then(hex),
                raw.get(idx + 2).copied().and_then(hex),
            ) {
                out.push(hi * 16 + lo);
                idx += 3;
                continue;
            }
        }
        out.push(*byte);
        idx += 1;
    }
    out
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| text.get(prefix.len()..))
        .flatten()
}

fn unquote(value: &str) -> String {
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    unquoted.replace("\\\"", "\"")
}

fn escape_param(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Base64 that ignores whitespace and stops at the first invalid byte, which is
/// how Python's `base64.decodebytes` behaves for these payloads.
fn decode_base64(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in raw {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            let shifted = acc >> bits;
            out.push(u8::try_from(shifted & 0xff).unwrap_or(0));
        }
    }
    out
}

fn decode_quoted_printable(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while let Some(byte) = raw.get(idx) {
        if *byte != b'=' {
            out.push(*byte);
            idx += 1;
            continue;
        }
        match (raw.get(idx + 1), raw.get(idx + 2)) {
            // A soft line break joins the next line.
            (Some(b'\n'), _) => idx += 2,
            (Some(b'\r'), Some(b'\n')) => idx += 3,
            (Some(hi), Some(lo)) => {
                if let (Some(hi), Some(lo)) = (hex(*hi), hex(*lo)) {
                    out.push(hi << 4 | lo);
                    idx += 3;
                } else {
                    out.push(*byte);
                    idx += 1;
                }
            }
            _ => {
                out.push(*byte);
                idx += 1;
            }
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
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

    #[test]
    fn reads_headers_and_body() {
        let msg = parse(b"Content-Type: text/plain\nX-Other: 1\n\nhello\n");
        assert_eq!(msg.content_type(), "text/plain");
        assert_eq!(msg.header("x-other"), Some("1"));
        assert_eq!(msg.decoded_payload(), b"hello\n");
    }

    #[test]
    fn folded_header_lines_are_joined() {
        let msg = parse(b"Content-Type: text/plain;\n  charset=\"utf-8\"\n\nx\n");
        assert_eq!(msg.content_type(), "text/plain");
        assert_eq!(
            param(msg.header("Content-Type").unwrap(), "charset").as_deref(),
            Some("utf-8")
        );
    }

    #[test]
    fn splits_a_multipart_body() {
        let raw =
            b"MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary=\"BOUND\"\n\n\
preamble\n\
--BOUND\n\
Content-Type: text/cloud-config\n\n\
#cloud-config\n\
--BOUND\n\
Content-Type: text/x-shellscript\n\n\
#!/bin/sh\n\
--BOUND--\n\
epilogue\n";
        let msg = parse(raw);
        assert!(msg.is_multipart());
        assert_eq!(msg.parts().len(), 2);
        assert_eq!(msg.parts()[0].content_type(), "text/cloud-config");
        assert_eq!(msg.parts()[0].decoded_payload(), b"#cloud-config");
        assert_eq!(msg.parts()[1].decoded_payload(), b"#!/bin/sh");
    }

    #[test]
    fn walk_yields_the_container_then_its_parts() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain\n\nx\n--B--\n";
        let msg = parse(raw);
        let walked = msg.walk();
        assert_eq!(walked.len(), 2);
        assert_eq!(walked[0].content_maintype(), "multipart");
        assert_eq!(walked[1].content_type(), "text/plain");
    }

    #[test]
    fn decodes_base64_transfer_encoding() {
        let msg = parse(
            b"Content-Type: text/plain\nContent-Transfer-Encoding: base64\n\nI2Nsb3VkLWNvbmZpZwo=\n",
        );
        assert_eq!(msg.decoded_payload(), b"#cloud-config\n");
    }

    #[test]
    fn decodes_quoted_printable_including_soft_breaks() {
        let msg = parse(
            b"Content-Type: text/plain\nContent-Transfer-Encoding: quoted-printable\n\na=3Db=\nc\n",
        );
        assert_eq!(msg.decoded_payload(), b"a=bc\n");
    }

    #[test]
    fn reads_a_filename_from_either_header() {
        let msg = parse(
            b"Content-Type: text/plain\nContent-Disposition: attachment; filename=\"a.txt\"\n\nx",
        );
        assert_eq!(msg.filename().as_deref(), Some("a.txt"));

        let msg = parse(b"Content-Type: text/plain; name=\"b.txt\"\n\nx");
        assert_eq!(msg.filename().as_deref(), Some("b.txt"));
    }

    #[test]
    fn nesting_stops_at_the_depth_limit() {
        let mut raw = String::new();
        for i in 0..(MAX_DEPTH + 4) {
            use std::fmt::Write as _;
            let _ = write!(
                raw,
                "Content-Type: multipart/mixed; boundary=\"B{i}\"\n\n--B{i}\n"
            );
        }
        let msg = parse(raw.as_bytes());
        let mut depth = 0;
        let mut cursor = &msg;
        while let Some(next) = cursor.parts().first() {
            depth += 1;
            cursor = next;
        }
        assert!(depth <= MAX_DEPTH, "{depth}");
    }

    #[test]
    fn a_line_that_is_not_a_header_starts_the_body() {
        // Python raises MissingHeaderBodySeparatorDefect and treats the whole
        // document as the payload.
        let msg = parse(b"#cloud-config\nruncmd: []\n");
        assert_eq!(msg.decoded_payload(), b"#cloud-config\nruncmd: []\n");
    }

    #[test]
    fn a_filename_can_use_rfc_2231_encoding() {
        // Every form Python's email package emits or accepts.
        for (header, want) in [
            ("attachment; filename=\"quoted name.sh\"", "quoted name.sh"),
            ("attachment; filename*=utf-8''%C3%A9.sh", "é.sh"),
            ("attachment; filename*=iso-8859-1''%E9.sh", "é.sh"),
            ("attachment; filename*0=abc; filename*1=def.sh", "abcdef.sh"),
            (
                "attachment; filename*0*=utf-8''%C3%A9; filename*1*=.sh",
                "é.sh",
            ),
        ] {
            let msg = parse(format!("Content-Disposition: {header}\n\nx").as_bytes());
            assert_eq!(msg.filename().as_deref(), Some(want), "{header}");
        }
    }

    #[test]
    fn a_parameter_sharing_a_prefix_is_not_mistaken_for_a_segment() {
        let msg = parse(b"Content-Disposition: attachment; filenamex=no.sh\n\nx");
        assert_eq!(msg.filename(), None);
    }
}
