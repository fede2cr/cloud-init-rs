//! Python `repr()` for JSON values.
//!
//! Every message the `jsonschema` library produces interpolates the offending
//! instance with `{instance!r}`, so `cloud-init schema` output cannot be matched
//! without reproducing `CPython`'s `repr` byte for byte.

use std::fmt::Write as _;

use serde_json::Value;

/// `repr(value)` as `CPython` would render the equivalent Python object.
pub fn repr(value: &Value) -> String {
    let mut out = String::new();
    write_repr(&mut out, value);
    out
}

fn write_repr(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => out.push_str(&repr_number(n)),
        Value::String(s) => write_repr_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr_str(out, key);
                out.push_str(": ");
                write_repr(out, val);
            }
            out.push('}');
        }
    }
}

/// `repr(float)` / `repr(int)`.
///
/// YAML integers stay integers on both sides, so the only interesting case is
/// the float formatting: `CPython` prints the shortest string that round-trips,
/// but always with a decimal point or an exponent, and switches to exponent
/// notation outside `1e-4 ..= 1e16`.
fn repr_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let Some(f) = n.as_f64() else {
        return n.to_string();
    };
    repr_float(f)
}

/// `repr()` of a Python float.
pub fn repr_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_owned();
    }

    // Rust's `{}` is also shortest-round-trip, so it agrees with `CPython` on the
    // digits; only the placement of the decimal point has to be fixed up.
    let magnitude = f.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        return exponent_form(f);
    }
    let plain = format!("{f}");
    if plain.contains(['.', 'e', 'n', 'i']) {
        plain
    } else {
        format!("{plain}.0")
    }
}

/// `CPython` renders exponents as `<mantissa>e[+-]NN`, with at least two exponent
/// digits and a mantissa that keeps its decimal point only when it has one.
fn exponent_form(f: f64) -> String {
    let formatted = format!("{f:e}");
    let Some((mantissa, exponent)) = formatted.split_once('e') else {
        return formatted;
    };
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exponent),
    };
    let mut out = mantissa.to_owned();
    let _ = write!(out, "e{sign}{digits:0>2}");
    out
}

/// `repr(str)`: single quotes unless the value contains one and no double quote.
fn write_repr_str(out: &mut String, s: &str) {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if needs_escape(c) => {
                let code = c as u32;
                if code < 0x100 {
                    let _ = write!(out, "\\x{code:02x}");
                } else if code < 0x1_0000 {
                    let _ = write!(out, "\\u{code:04x}");
                } else {
                    let _ = write!(out, "\\U{code:08x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
}

/// `CPython` escapes anything `str.isprintable()` rejects: C0/C1 controls, all
/// separators except a plain space, surrogates, and unassigned code points.
///
/// Only the ranges reachable from YAML config are distinguished here; every
/// non-ASCII character that a YAML file can carry and that `CPython` considers
/// printable is passed through unchanged.
fn needs_escape(c: char) -> bool {
    let code = c as u32;
    if c == ' ' {
        return false;
    }
    if code < 0x20 || code == 0x7f {
        return true;
    }
    if (0x80..0xa0).contains(&code) {
        return true;
    }
    matches!(
        c,
        '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}'
    ) || c.is_whitespace() && code > 0x7f
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
    use serde_json::json;

    fn r(v: &serde_json::Value) -> String {
        repr(v)
    }

    #[test]
    fn renders_scalars_like_python() {
        assert_eq!(r(&json!(null)), "None");
        assert_eq!(r(&json!(true)), "True");
        assert_eq!(r(&json!(false)), "False");
        assert_eq!(r(&json!(5)), "5");
        assert_eq!(r(&json!(-17)), "-17");
        assert_eq!(r(&json!("hi")), "'hi'");
    }

    #[test]
    fn renders_floats_like_python() {
        assert_eq!(repr_float(5.0), "5.0");
        assert_eq!(repr_float(0.5), "0.5");
        assert_eq!(repr_float(-2.25), "-2.25");
        assert_eq!(repr_float(1e16), "1e+16");
        assert_eq!(repr_float(1e-5), "1e-05");
        assert_eq!(repr_float(1.5e-7), "1.5e-07");
        assert_eq!(repr_float(0.0001), "0.0001");
        assert_eq!(repr_float(0.0), "0.0");
        assert_eq!(repr_float(1e300), "1e+300");
        assert_eq!(repr_float(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(repr_float(5e-324), "5e-324");
        assert_eq!(
            repr_float(123_456_789_012_345_680.0),
            "1.2345678901234568e+17"
        );
        assert_eq!(repr_float(std::f64::consts::PI), "3.141592653589793");
    }

    #[test]
    fn picks_the_quote_python_would_pick() {
        assert_eq!(r(&json!("it's")), "\"it's\"");
        assert_eq!(r(&json!("say \"hi\"")), "'say \"hi\"'");
        assert_eq!(r(&json!("both ' and \"")), "'both \\' and \"'");
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(r(&json!("a\nb")), "'a\\nb'");
        assert_eq!(r(&json!("a\tb")), "'a\\tb'");
        assert_eq!(r(&json!("a\\b")), "'a\\\\b'");
        assert_eq!(r(&json!("a\u{7}b")), "'a\\x07b'");
        assert_eq!(r(&json!("a\u{0}b")), "'a\\x00b'");
        assert_eq!(r(&json!("a\u{7f}b")), "'a\\x7fb'");
        assert_eq!(r(&json!("\u{85}")), "'\\x85'");
        assert_eq!(r(&json!("\u{ad}")), "'\\xad'");
        assert_eq!(r(&json!("\u{200b}")), "'\\u200b'");
        assert_eq!(r(&json!("\u{2028}")), "'\\u2028'");
        assert_eq!(r(&json!("\u{feff}")), "'\\ufeff'");
    }

    #[test]
    fn passes_printable_non_ascii_through() {
        assert_eq!(r(&json!("caf\u{e9}")), "'caf\u{e9}'");
        assert_eq!(r(&json!("日本語")), "'日本語'");
    }

    #[test]
    fn renders_containers_in_insertion_order() {
        assert_eq!(r(&json!([1, "a", null])), "[1, 'a', None]");
        assert_eq!(r(&json!([])), "[]");
        assert_eq!(r(&json!({})), "{}");
        assert_eq!(r(&json!({"b": 1, "a": [true]})), "{'b': 1, 'a': [True]}");
    }
}
