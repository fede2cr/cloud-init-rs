//! `PyYAML`'s YAML 1.1 implicit resolver and scalar constructors.
//!
//! Upstream loads every config through `yaml.SafeLoader`, which follows YAML 1.1.
//! Rust YAML parsers follow the 1.2 core schema, and the two disagree on forms
//! that are common in cloud-config: `yes`/`no` are booleans in 1.1 but strings in
//! 1.2, `0600` is octal in 1.1 but not a number at all in 1.2, and `0o600` is a
//! number in 1.2 but a plain string in 1.1. Getting this wrong silently changes
//! the type of a setting, so the resolver is reproduced here rather than
//! approximated. See `docs/COMPAT.md`.
//!
//! Ported from `yaml/resolver.py` and `SafeConstructor` in `yaml/constructor.py`.

use crate::Value;

/// The YAML 1.1 type a plain scalar resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tag {
    Null,
    Bool,
    Int,
    Float,
    Timestamp,
    /// `<<`, the merge key.
    Merge,
    /// `=`, which `SafeLoader` has no constructor for.
    Value,
    Str,
}

/// Resolve a plain (unquoted, untagged) scalar the way `PyYAML` does.
///
/// Quoted and block scalars never reach here: they are always strings.
pub(super) fn resolve(text: &str) -> Tag {
    // PyYAML dispatches on the first character before trying any regex, so an
    // empty scalar only ever matches the null resolver.
    let Some(first) = text.chars().next() else {
        return Tag::Null;
    };
    match first {
        '~' | 'n' | 'N' if is_null(text) => Tag::Null,
        'y' | 'Y' | 't' | 'T' | 'f' | 'F' | 'o' | 'O' | 'n' | 'N' if is_bool(text) => {
            Tag::Bool
        }
        '<' if text == "<<" => Tag::Merge,
        '=' if text == "=" => Tag::Value,
        '-' | '+' | '.' | '0'..='9' => {
            if is_float(text) {
                Tag::Float
            } else if is_int(text) {
                Tag::Int
            } else if is_timestamp(text) {
                Tag::Timestamp
            } else {
                Tag::Str
            }
        }
        _ => Tag::Str,
    }
}

fn is_null(text: &str) -> bool {
    matches!(text, "~" | "null" | "Null" | "NULL")
}

fn is_bool(text: &str) -> bool {
    matches!(
        text,
        "yes"
            | "Yes"
            | "YES"
            | "no"
            | "No"
            | "NO"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// `true` for the `yes`/`true`/`on` family; only call on a [`Tag::Bool`] scalar.
pub(super) fn bool_value(text: &str) -> bool {
    matches!(
        text,
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON"
    )
}

/// `^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$`
fn is_int(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    if let Some(bits) = body.strip_prefix("0b") {
        return !bits.is_empty() && bits.chars().all(|c| matches!(c, '0' | '1' | '_'));
    }
    if let Some(hex) = body.strip_prefix("0x") {
        return !hex.is_empty()
            && hex.chars().all(|c| c.is_ascii_hexdigit() || c == '_');
    }
    if body == "0" {
        return true;
    }
    if let Some(rest) = body.strip_prefix('0') {
        // Leading zero means octal in YAML 1.1; `0o600` is deliberately not
        // matched here, which is why upstream reads it as a string.
        return !rest.is_empty() && rest.chars().all(|c| matches!(c, '0'..='7' | '_'));
    }
    if is_sexagesimal(body, false) {
        return true;
    }
    body.starts_with(|c: char| c.is_ascii_digit())
        && body.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// ```text
/// ^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?
///   |\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?
///   |[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*
///   |[-+]?\.(?:inf|Inf|INF)
///   |\.(?:nan|NaN|NAN))$
/// ```
fn is_float(text: &str) -> bool {
    if matches!(text, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let signed = text.starts_with(['-', '+']);
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    if matches!(body, ".inf" | ".Inf" | ".INF") {
        return true;
    }
    let Some((mantissa, has_exponent)) = split_exponent(body) else {
        return false;
    };
    let Some((int_part, frac_part)) = mantissa.split_once('.') else {
        return false;
    };
    if !frac_part.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return false;
    }
    if int_part.is_empty() {
        // `.5` — the leading-dot branch carries no sign and needs a digit.
        return !signed && frac_part.starts_with(|c: char| c.is_ascii_digit());
    }
    // The base-60 branch has no exponent.
    if !has_exponent && is_sexagesimal(int_part, true) {
        return true;
    }
    int_part.starts_with(|c: char| c.is_ascii_digit())
        && int_part.chars().all(|c| c.is_ascii_digit() || c == '_')
}

/// Split a trailing `[eE][-+][0-9]+`. Returns `None` if an exponent marker is
/// present but malformed; `PyYAML` requires an explicit sign there.
fn split_exponent(body: &str) -> Option<(&str, bool)> {
    let Some(pos) = body.rfind(['e', 'E']) else {
        return Some((body, false));
    };
    let (mantissa, exp) = body.split_at(pos);
    let digits = exp
        .get(1..)
        .and_then(|rest| rest.strip_prefix(['-', '+']))?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((mantissa, true))
}

/// `[0-9][0-9_]*(?::[0-5]?[0-9])+` — base-60, as in `12:30`.
fn is_sexagesimal(body: &str, allow_leading_zero: bool) -> bool {
    let mut parts = body.split(':');
    let Some(head) = parts.next() else {
        return false;
    };
    let mut any = false;
    for part in parts {
        any = true;
        let ok = match part.len() {
            1 => part.starts_with(|c: char| c.is_ascii_digit()),
            2 => {
                part.starts_with(|c: char| ('0'..='5').contains(&c))
                    && part.ends_with(|c: char| c.is_ascii_digit())
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    if !any || head.is_empty() {
        return false;
    }
    if !allow_leading_zero && !head.starts_with(|c: char| ('1'..='9').contains(&c)) {
        return false;
    }
    head.chars().all(|c| c.is_ascii_digit() || c == '_')
}

fn is_timestamp(text: &str) -> bool {
    // Only the plain `YYYY-MM-DD` form and the full date-time form resolve; the
    // port treats both as timestamps and then falls back to a string.
    let digits_and_dashes = |pattern: &str| {
        text.len() == pattern.len()
            && text.chars().zip(pattern.chars()).all(|(c, p)| match p {
                '-' => c == '-',
                _ => c.is_ascii_digit(),
            })
    };
    if digits_and_dashes("0000-00-00") {
        return true;
    }
    let mut chars = text.chars();
    let year = (0..4).all(|_| chars.next().is_some_and(|c| c.is_ascii_digit()));
    year && text.len() > 10 && text.contains('-')
}

/// `construct_yaml_int`: strip `_`, then honour the `0b`/`0x`/`0`/base-60 forms.
pub(super) fn int_value(text: &str) -> Option<Value> {
    let stripped = text.replace('_', "");
    let (sign, body) = match stripped.strip_prefix(['-', '+']) {
        Some(rest) => (if stripped.starts_with('-') { -1i64 } else { 1 }, rest),
        None => (1, stripped.as_str()),
    };
    let magnitude = if body == "0" {
        Some(0)
    } else if let Some(bits) = body.strip_prefix("0b") {
        i64::from_str_radix(bits, 2).ok()
    } else if let Some(hex) = body.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()
    } else if body.starts_with('0') {
        i64::from_str_radix(body, 8).ok()
    } else if body.contains(':') {
        base60(body)
    } else {
        body.parse::<i64>().ok()
    };
    magnitude.map(|m| Value::from(sign * m))
}

/// `construct_yaml_float`: strip `_`, lowercase, then handle `.inf`/`.nan`/base-60.
///
/// Returns `None` for the non-finite forms, which JSON cannot represent.
pub(super) fn float_value(text: &str) -> Option<Value> {
    let lowered = text.replace('_', "").to_lowercase();
    let (sign, body) = match lowered.strip_prefix(['-', '+']) {
        Some(rest) => (
            if lowered.starts_with('-') {
                -1.0f64
            } else {
                1.0
            },
            rest,
        ),
        None => (1.0, lowered.as_str()),
    };
    if body == ".inf" || body == ".nan" {
        return None;
    }
    let magnitude = if body.contains(':') {
        let mut total = 0.0f64;
        let mut base = 1.0f64;
        for part in body.rsplit(':') {
            total += part.parse::<f64>().ok()? * base;
            base *= 60.0;
        }
        total
    } else {
        body.parse::<f64>().ok()?
    };
    serde_json::Number::from_f64(sign * magnitude).map(Value::Number)
}

fn base60(body: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut base: i64 = 1;
    for part in body.rsplit(':') {
        total = total.checked_add(part.parse::<i64>().ok()?.checked_mul(base)?)?;
        base = base.checked_mul(60)?;
    }
    Some(total)
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
    fn resolves_the_yaml_1_1_boolean_family() {
        for text in ["yes", "Yes", "YES", "no", "off", "ON", "true", "False"] {
            assert_eq!(resolve(text), Tag::Bool, "{text}");
        }
        assert!(bool_value("yes"));
        assert!(bool_value("On"));
        assert!(!bool_value("off"));
        assert!(!bool_value("NO"));
    }

    #[test]
    fn treats_a_leading_zero_as_octal_but_not_the_0o_form() {
        assert_eq!(resolve("0600"), Tag::Int);
        assert_eq!(int_value("0600"), Some(Value::from(384)));
        assert_eq!(resolve("0o600"), Tag::Str);
    }

    #[test]
    fn resolves_the_other_integer_bases() {
        assert_eq!(int_value("0x1F"), Some(Value::from(31)));
        assert_eq!(int_value("0b101"), Some(Value::from(5)));
        assert_eq!(int_value("1_000"), Some(Value::from(1000)));
        assert_eq!(int_value("-17"), Some(Value::from(-17)));
    }

    #[test]
    fn resolves_base_sixty() {
        assert_eq!(resolve("12:30"), Tag::Int);
        assert_eq!(int_value("12:30"), Some(Value::from(750)));
    }

    #[test]
    fn resolves_floats_and_rejects_unsigned_exponents() {
        assert_eq!(resolve("1.5"), Tag::Float);
        assert_eq!(resolve("1.5e+3"), Tag::Float);
        // PyYAML's float regex requires a sign in the exponent.
        assert_eq!(resolve("1.5e3"), Tag::Str);
        assert_eq!(float_value("1.5"), Some(Value::from(1.5)));
    }

    #[test]
    fn non_finite_floats_have_no_json_representation() {
        assert_eq!(resolve(".inf"), Tag::Float);
        assert_eq!(float_value(".inf"), None);
        assert_eq!(float_value(".nan"), None);
    }

    #[test]
    fn resolves_nulls_merges_and_values() {
        assert_eq!(resolve(""), Tag::Null);
        assert_eq!(resolve("~"), Tag::Null);
        assert_eq!(resolve("NULL"), Tag::Null);
        assert_eq!(resolve("<<"), Tag::Merge);
        assert_eq!(resolve("="), Tag::Value);
    }

    #[test]
    fn resolves_plain_dates_as_timestamps() {
        assert_eq!(resolve("2030-01-02"), Tag::Timestamp);
        assert_eq!(resolve("2030-01-0"), Tag::Str);
    }

    #[test]
    fn leaves_ordinary_words_alone() {
        for text in ["hostname", "git", "/dev/vdb", "echo hi", "0600x"] {
            assert_eq!(resolve(text), Tag::Str, "{text}");
        }
    }
}
