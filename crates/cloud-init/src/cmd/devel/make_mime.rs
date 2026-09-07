//! `cloud-init devel make-mime` — build a multipart user-data archive.
//!
//! Port of `cloudinit/cmd/devel/make_mime.py`.

use std::fmt::Write as _;
use std::path::Path;

use clap::Args as ClapArgs;

/// `handlers.INCLUSION_TYPES_MAP` values, sorted, as `get_content_types()` returns them.
const CONTENT_TYPES: [&str; 12] = [
    "text/cloud-boothook",
    "text/cloud-config",
    "text/cloud-config-archive",
    "text/cloud-config-jsonp",
    "text/jinja2",
    "text/part-handler",
    "text/x-include-once-url",
    "text/x-include-url",
    "text/x-shellscript",
    "text/x-shellscript-per-boot",
    "text/x-shellscript-per-instance",
    "text/x-shellscript-per-once",
];

const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Attach the given file as the specified content-type.
    #[arg(long, short = 'a', value_name = "<file>:<content-type>")]
    pub attach: Vec<String>,

    /// List support cloud-init content types.
    #[arg(long = "list-types", short = 'l')]
    pub list_types: bool,

    /// Ignore unknown content-type warnings.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Debug)]
struct Attachment {
    filename: String,
    content_type: String,
    contents: String,
}
pub fn run(args: &Args) -> u8 {
    // argparse applies the `-a` type conversion, which opens each file, before
    // `handle_args` looks at `--list-types`.
    let mut attachments = Vec::with_capacity(args.attach.len());
    for spec in &args.attach {
        match parse_attachment(spec) {
            Ok(attachment) => attachments.push(attachment),
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        }
    }

    if args.list_types {
        for ctype in CONTENT_TYPES {
            println!("{}", ctype.replace("text/", ""));
        }
        return 0;
    }

    let errors: Vec<String> = attachments
        .iter()
        .enumerate()
        .filter(|(_, a)| !CONTENT_TYPES.contains(&a.content_type.as_str()))
        .map(|(i, a)| {
            format!(
                "content type {} for attachment {} may be incorrect!",
                python_repr(&a.content_type),
                i.saturating_add(1)
            )
        })
        .collect();

    if !errors.is_empty() {
        let level = if args.force { "WARNING" } else { "ERROR" };
        for error in &errors {
            eprintln!("{level}: {error}");
        }
        eprintln!("Invalid content-types, override with --force");
        if !args.force {
            return 1;
        }
    }

    match render_message(&attachments) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// `file_content_type()`. Upstream lets a missing file escape as a traceback
/// (docs/COMPAT.md B8); the split failure crashes argparse itself (B7).
fn parse_attachment(spec: &str) -> Result<Attachment, String> {
    let Some((filename, content_type)) = spec.split_once(':') else {
        return Err(format!("Invalid value for {}", python_repr(spec)));
    };
    let contents =
        ci_sys::path::read_text_capped(Path::new(filename), MAX_ATTACHMENT_BYTES)
            .map_err(|e| format!("Cannot read attachment {filename}: {e}"))?;
    Ok(Attachment {
        filename: filename.to_owned(),
        content_type: format!("text/{}", content_type.trim()),
        contents,
    })
}

fn render_message(attachments: &[Attachment]) -> Result<String, String> {
    let parts: Vec<String> = attachments
        .iter()
        .map(|a| {
            format!(
                "Content-Type: {ctype}; charset=\"utf-8\"\n\
                 MIME-Version: 1.0\n\
                 Content-Transfer-Encoding: base64\n\
                 Content-Disposition: attachment; filename=\"{name}\"\n\n{body}",
                ctype = a.content_type,
                name = a.filename,
                body = base64_mime(a.contents.as_bytes()),
            )
        })
        .collect();

    let boundary = make_boundary(&parts)?;
    let mut out = format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\nMIME-Version: 1.0\n\n"
    );
    let _ = writeln!(out, "--{boundary}");
    out.push_str(&parts.join(&format!("\n--{boundary}\n")));
    let _ = writeln!(out, "\n--{boundary}--");
    Ok(out)
}

/// `email.generator._make_boundary()`: 15 `=`, a zero-padded 19-digit value below
/// `sys.maxsize`, then `==`, retried until it does not occur in the body.
fn make_boundary(parts: &[String]) -> Result<String, String> {
    let body = parts.join("");
    for _ in 0..64 {
        let token = ci_sys::random_u64()
            .map_err(|e| format!("Cannot read /dev/urandom: {e}"))?
            % (i64::MAX as u64);
        let candidate = format!("==============={token:019}==");
        if !body.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err("Unable to generate a MIME boundary".to_owned())
}

const B64: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64.encodebytes()`: 76 characters per line, every line newline-terminated.
fn base64_mime(data: &[u8]) -> String {
    let mut out = String::new();
    let mut column: usize = 0;
    for chunk in data.chunks(3) {
        let [b0, b1, b2] = match chunk {
            [b0] => [*b0, 0, 0],
            [b0, b1] => [*b0, *b1, 0],
            [b0, b1, b2, ..] => [*b0, *b1, *b2],
            [] => break,
        };
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        for i in 0..4_u32 {
            let sextet = (n >> (18 - i * 6)) & 0x3f;
            let keep = u32::try_from(chunk.len()).unwrap_or(3).saturating_add(1);
            out.push(if i < keep {
                char::from(B64.get(sextet as usize).copied().unwrap_or(b'='))
            } else {
                '='
            });
        }
        column = column.saturating_add(4);
        if column >= 76 {
            out.push('\n');
            column = 0;
        }
    }
    if column > 0 {
        out.push('\n');
    }
    out
}

/// `repr()` of a `str`: single quotes unless the value contains one and no double.
fn python_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', "\\'"))
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

    fn attachment(name: &str, ctype: &str, body: &str) -> Attachment {
        Attachment {
            filename: name.to_owned(),
            content_type: ctype.to_owned(),
            contents: body.to_owned(),
        }
    }

    fn normalize(message: &str) -> String {
        let mut out = String::new();
        let mut rest = message;
        while let Some(idx) = rest.find("===============") {
            out.push_str(&rest[..idx]);
            out.push_str("BOUNDARY");
            rest = &rest[idx.saturating_add(15).saturating_add(19).saturating_add(2)..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn encodes_base64_the_way_the_email_module_does() {
        assert_eq!(base64_mime(b""), "");
        assert_eq!(base64_mime(b"a"), "YQ==\n");
        assert_eq!(base64_mime(b"ab"), "YWI=\n");
        assert_eq!(base64_mime(b"abc"), "YWJj\n");
        assert_eq!(
            base64_mime(b"#cloud-config\nruncmd: [echo hi]\n"),
            "I2Nsb3VkLWNvbmZpZwpydW5jbWQ6IFtlY2hvIGhpXQo=\n"
        );
    }

    #[test]
    fn wraps_base64_at_seventy_six_columns() {
        let encoded = base64_mime(&b"x".repeat(200));
        for line in encoded.lines() {
            assert!(line.len() <= 76, "line too long: {}", line.len());
        }
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded.lines().next().unwrap().len(), 76);
    }

    #[test]
    fn renders_an_empty_archive_like_an_empty_multipart() {
        let message = render_message(&[]).unwrap();
        assert_eq!(
            normalize(&message),
            "Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\n\
             MIME-Version: 1.0\n\n--BOUNDARY\n\n--BOUNDARY--\n"
        );
    }

    #[test]
    fn renders_one_part_with_the_upstream_header_order() {
        let parts = [attachment("c.yaml", "text/cloud-config", "#cloud-config\n")];
        assert_eq!(
            normalize(&render_message(&parts).unwrap()),
            "Content-Type: multipart/mixed; boundary=\"BOUNDARY\"\n\
             MIME-Version: 1.0\n\n\
             --BOUNDARY\n\
             Content-Type: text/cloud-config; charset=\"utf-8\"\n\
             MIME-Version: 1.0\n\
             Content-Transfer-Encoding: base64\n\
             Content-Disposition: attachment; filename=\"c.yaml\"\n\n\
             I2Nsb3VkLWNvbmZpZwo=\n\n\
             --BOUNDARY--\n"
        );
    }

    #[test]
    fn separates_multiple_parts_with_a_blank_line_before_each_boundary() {
        let parts = [
            attachment("a", "text/cloud-config", "a\n"),
            attachment("b", "text/x-shellscript", "b\n"),
        ];
        let message = normalize(&render_message(&parts).unwrap());
        assert!(
            message.contains("YQo=\n\n--BOUNDARY\nContent-Type: text/x-shellscript")
        );
        assert!(message.ends_with("Ygo=\n\n--BOUNDARY--\n"));
    }

    #[test]
    fn the_boundary_has_the_shape_the_email_module_produces() {
        let boundary = make_boundary(&[]).unwrap();
        assert_eq!(boundary.len(), 15 + 19 + 2);
        assert!(boundary.starts_with("==============="));
        assert!(boundary.ends_with("=="));
        let digits = &boundary[15..34];
        assert!(digits.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(boundary, make_boundary(&[]).unwrap());
    }

    #[test]
    fn rejects_an_attachment_without_a_content_type() {
        assert_eq!(
            parse_attachment("nocolon").unwrap_err(),
            "Invalid value for 'nocolon'"
        );
    }

    #[test]
    fn strips_the_text_prefix_when_listing_types() {
        assert_eq!(CONTENT_TYPES[0].replace("text/", ""), "cloud-boothook");
        assert_eq!(CONTENT_TYPES.len(), 12);
    }

    #[test]
    fn quotes_like_python_repr() {
        assert_eq!(python_repr("plain"), "'plain'");
        assert_eq!(python_repr("it's"), "\"it's\"");
    }

    #[test]
    fn reads_an_attachment_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(&path, "#cloud-config\n").unwrap();
        let spec = format!("{}:cloud-config", path.display());
        let attachment = parse_attachment(&spec).unwrap();
        assert_eq!(attachment.content_type, "text/cloud-config");
        assert_eq!(attachment.contents, "#cloud-config\n");
    }
}
