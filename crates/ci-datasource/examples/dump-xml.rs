//! Dump `ET.tostring` round trips and the Azure password redaction, for
//! differential testing.
//!
//! Usage: `dump-xml <file>`
//!
//! The file holds one base64 blob per line, so a case can carry newlines,
//! quotes and non-ASCII without the shell in between having an opinion.

use ci_config::{Object, Value};

/// `write_files._redact_password`, over the parsed tree.
fn redact(element: &mut ci_config::xml::Element) {
    if element.tag().contains("UserPassword")
        && element.text.as_deref() != Some("REDACTED")
    {
        element.text = Some("REDACTED".to_owned());
    }
    for child in &mut element.children {
        redact(child);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: dump-xml <file>");
        std::process::exit(2);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("cannot read {path}");
        std::process::exit(2);
    };

    let limits = ci_config::xml::Limits::default();
    let mut out = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let Some(raw) = ci_core::b64::decode(line) else {
            continue;
        };
        let input = String::from_utf8_lossy(&raw).into_owned();

        let mut case = Object::new();
        // The base64, not the text: a case can carry non-ASCII, and the two
        // JSON encoders do not agree about how to spell it (`ensure_ascii`).
        // What is under test is the XML, so the label stays ASCII.
        case.insert("case".to_owned(), Value::from(line));
        if let Ok(root) = ci_config::xml::parse(&input, limits) {
            let mut redacted = root.clone();
            redact(&mut redacted);
            for (key, tree) in [("tostring", &root), ("redacted", &redacted)] {
                let bytes = ci_config::xml::serialize(tree);
                case.insert(
                    key.to_owned(),
                    Value::from(String::from_utf8_lossy(&bytes).into_owned()),
                );
            }
        } else {
            // The two parsers reject different documents with different
            // words; the comparison is only over what both accept.
            case.insert("tostring".to_owned(), Value::Null);
            case.insert("redacted".to_owned(), Value::Null);
        }
        out.push(Value::Object(case));
    }

    println!("{}", ci_core::jsonfmt::dumps_indent(&Value::Array(out), 1));
}
