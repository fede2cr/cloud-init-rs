//! Python-compatible JSON serialisation.
//!
//! Upstream emits `json.dumps(obj, indent=N, sort_keys=True, separators=(",", ": "))`.
//! `serde_json`'s pretty printer defaults to two-space indentation and preserves
//! insertion order, so both the indent width and the key ordering have to be
//! matched explicitly for output to be diffable against Python cloud-init.

use std::fmt::Write as _;

use serde_json::{Number, Value};

/// `util.json_dumps()` — indent 1, sorted keys.
pub fn json_dumps(value: &Value) -> String {
    dumps_indent(value, 1)
}

/// `json.dumps(value, separators=(",", ":"))`.
///
/// `CPython`'s defaults are `ensure_ascii=True` and insertion order, neither of
/// which `serde_json` does: it emits UTF-8 verbatim, and its `Value` only keeps
/// insertion order because this workspace turns `preserve_order` on. Anything
/// compared against Python byte-for-byte has to go through here.
#[must_use]
pub fn dumps_compact(value: &Value) -> String {
    let mut out = String::new();
    write_compact(value, ",", ":", &mut out);
    out
}

/// `json.dumps(value)` with `CPython`'s defaults, whose separators are
/// `(", ", ": ")` rather than the tight pair [`dumps_compact`] uses.
#[must_use]
pub fn dumps_default(value: &Value) -> String {
    let mut out = String::new();
    write_compact(value, ", ", ": ", &mut out);
    out
}

/// `json.dumps(text)` for a string: the quoted, ASCII-escaped form.
#[must_use]
pub fn quote_string(text: &str) -> String {
    let mut out = String::new();
    write_quoted(text, &mut out);
    out
}

/// Python's `str(float)`/`repr(float)`, which keeps a trailing `.0` that Rust
/// drops and switches to exponent notation at both ends of the range.
///
/// Python moves to exponents below `1e-4` and at or above `1e16`; Rust's `{}`
/// never does, and its `{:e}` writes a bare, unpadded exponent, so both ends
/// have to be assembled by hand.
#[must_use]
pub fn py_float(value: f64) -> String {
    let magnitude = value.abs();
    if value != 0.0 && magnitude.is_finite() && (magnitude < 1e-4 || magnitude >= 1e16)
    {
        return exponent_form(value);
    }
    if value.is_finite() && value.fract() == 0.0 {
        return format!("{value:.1}");
    }
    format!("{value}")
}

/// `{:e}` with Python's sign and two-digit minimum on the exponent.
fn exponent_form(value: f64) -> String {
    let shown = format!("{value:e}");
    let Some((mantissa, exponent)) = shown.split_once('e') else {
        return shown;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return shown;
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

fn write_compact(value: &Value, comma: &str, colon: &str, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&write_number(number)),
        Value::String(text) => write_quoted(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(comma);
                }
                write_compact(item, comma, colon, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(comma);
                }
                write_quoted(key, out);
                out.push_str(colon);
                write_compact(item, comma, colon, out);
            }
            out.push('}');
        }
    }
}

fn write_number(number: &Number) -> String {
    if number.is_f64() {
        number.as_f64().map_or_else(|| number.to_string(), py_float)
    } else {
        number.to_string()
    }
}

/// `json.encoder.py_encode_basestring_ascii`: everything outside `\x20`–`\x7e`
/// becomes a `\uXXXX` escape, with a surrogate pair above the BMP.
fn write_quoted(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => write_escape(ch, out),
        }
    }
    out.push('"');
}

fn write_escape(ch: char, out: &mut String) {
    let point = ch as u32;
    if let Some(above_bmp) = point.checked_sub(0x1_0000) {
        let high = 0xd800 + (above_bmp >> 10);
        let low = 0xdc00 + (above_bmp & 0x3ff);
        let _ = write!(out, "\\u{high:04x}\\u{low:04x}");
    } else {
        let _ = write!(out, "\\u{point:04x}");
    }
}

/// `util.load_json()`, minus the `root=` type check no caller uses.
///
/// Every caller treats malformed JSON the same way it treats a missing file,
/// so the parse error carries nothing worth reporting and is dropped.
#[must_use]
pub fn json_loads(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

/// `json.dumps(..., indent=n, sort_keys=True, separators=(",", ": "))`.
#[must_use]
pub fn dumps_indent(value: &Value, indent: usize) -> String {
    let sorted = sort_keys(value);
    let mut out = String::new();
    write_pretty(&sorted, indent, 0, &mut out);
    out
}

/// The pretty printer, written out rather than borrowed from `serde_json`
/// because Python escapes non-ASCII and `serde_json` does not.
///
/// Python's `indent=` also omits the space after the comma, and prints empty
/// containers on one line.
fn write_pretty(value: &Value, indent: usize, level: usize, out: &mut String) {
    match value {
        Value::Array(items) if !items.is_empty() => {
            let inner = " ".repeat(indent * (level + 1));
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&inner);
                write_pretty(item, indent, level + 1, out);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push(']');
        }
        Value::Object(map) if !map.is_empty() => {
            let inner = " ".repeat(indent * (level + 1));
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&inner);
                write_quoted(key, out);
                out.push_str(": ");
                write_pretty(item, indent, level + 1, out);
            }
            out.push('\n');
            out.push_str(&" ".repeat(indent * level));
            out.push('}');
        }
        other => write_compact(other, ", ", ": ", out),
    }
}

/// Recursively reorder mappings by key, emulating `sort_keys=True`.
pub fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for key in keys {
                if let Some(v) = map.get(key) {
                    out.insert(key.clone(), sort_keys(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
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
    use serde_json::json;

    /// Every expectation here is `repr(v)` from the packaged interpreter.
    #[test]
    fn py_float_switches_to_exponents_where_python_does() {
        let cases: [(f64, &str); 16] = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (1.5, "1.5"),
            (2.5, "2.5"),
            (100.0, "100.0"),
            // The small-magnitude switch is at 1e-4: this side stays plain.
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-0.00001, "-1e-05"),
            (9.536_743_164_062_5e-7, "9.5367431640625e-07"),
            (1.907_348_632_812_5e-6, "1.9073486328125e-06"),
            (1e-300, "1e-300"),
            // The large-magnitude switch is at 1e16.
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e20, "1.5e+20"),
            (1.234_567_890_123_456_7e16, "1.2345678901234568e+16"),
        ];
        for (value, want) in cases {
            assert_eq!(py_float(value), want, "{value}");
        }
    }

    #[test]
    fn matches_python_indent_one_sorted() {
        let value = json!({"b": 2, "a": {"d": 4, "c": [1, 2]}});
        assert_eq!(
            json_dumps(&value),
            "{\n \"a\": {\n  \"c\": [\n   1,\n   2\n  ],\n  \"d\": 4\n },\n \"b\": 2\n}"
        );
    }

    #[test]
    fn supports_indent_two_for_status_output() {
        let value = json!({"status": "done"});
        assert_eq!(dumps_indent(&value, 2), "{\n  \"status\": \"done\"\n}");
    }

    #[test]
    fn the_compact_form_keeps_insertion_order_and_drops_every_space() {
        let value = json!({"name": "n", "type": "start", "msg": ""});
        assert_eq!(
            dumps_compact(&value),
            "{\"name\":\"n\",\"type\":\"start\",\"msg\":\"\"}"
        );
    }

    #[test]
    fn the_compact_form_escapes_everything_outside_printable_ascii() {
        for (input, expected) in [
            ("caf\u{e9}", r#""caf\u00e9""#),
            ("\u{1f389}", r#""\ud83c\udf89""#),
            ("a\u{7f}b", r#""a\u007fb""#),
            ("a\u{1}b", r#""a\u0001b""#),
            ("tab\there", r#""tab\there""#),
            ("q\"b\\s", r#""q\"b\\s""#),
            ("\u{8}\u{c}\r\n", r#""\b\f\r\n""#),
        ] {
            assert_eq!(quote_string(input), expected, "{input:?}");
        }
    }

    #[test]
    fn a_whole_number_float_keeps_the_python_trailing_zero() {
        assert_eq!(dumps_compact(&json!({"d": 12.0})), "{\"d\":12.0}");
        assert_eq!(dumps_compact(&json!({"d": 0.0535})), "{\"d\":0.0535}");
        assert_eq!(dumps_compact(&json!({"i": 12})), "{\"i\":12}");
        assert_eq!(dumps_compact(&json!({"i": -1})), "{\"i\":-1}");
    }

    #[test]
    fn the_compact_form_covers_the_remaining_json_shapes() {
        assert_eq!(
            dumps_compact(&json!({"a": [1, null, true, false]})),
            "{\"a\":[1,null,true,false]}"
        );
    }
}
