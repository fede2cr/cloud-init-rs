//! Port of `util.get_cfg_option_*` and the `translate_bool`/`is_true` pair.
//!
//! These are not plain Python truthiness, and the difference bites: `2`, `1.0`
//! and `"maybe"` are all truthy to Python but false to [`translate_bool`], while
//! `1` and `"YES"` are true. Config modules gate destructive work on these, so
//! the rules are reproduced exactly rather than approximated.

use crate::{Object, Value};

/// `util.TRUE_STRINGS`.
const TRUE_STRINGS: [&str; 4] = ["true", "1", "on", "yes"];

/// `util.FALSE_STRINGS`.
const FALSE_STRINGS: [&str; 4] = ["off", "0", "no", "false"];

/// `util.is_true`.
#[must_use]
pub fn is_true(value: &Value) -> bool {
    if let Value::Bool(flag) = value {
        return *flag;
    }
    TRUE_STRINGS.contains(&scalar_str(value).trim().to_lowercase().as_str())
}

/// `util.is_false`.
///
/// Not the negation of [`is_true`]: `2`, `"maybe"` and `1.0` are neither, and
/// the caller that asks both is expected to have a third branch.
#[must_use]
pub fn is_false(value: &Value) -> bool {
    if let Value::Bool(flag) = value {
        return !*flag;
    }
    FALSE_STRINGS.contains(&scalar_str(value).trim().to_lowercase().as_str())
}

/// `util.translate_bool`.
#[must_use]
pub fn translate_bool(value: &Value) -> bool {
    if !py_truthy(value) {
        return false;
    }
    is_true(value)
}

/// `util.get_cfg_option_bool`.
#[must_use]
pub fn get_bool(cfg: &Object, key: &str, default: bool) -> bool {
    cfg.get(key).map_or(default, translate_bool)
}

/// `util.get_cfg_option_str`.
#[must_use]
pub fn get_str<'a>(cfg: &'a Object, key: &str) -> Option<&'a str> {
    cfg.get(key).map(Value::as_str)?
}

/// Plain `bool(value)` as Python computes it.
#[must_use]
pub fn py_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `str(value)` for the scalars upstream can reach here.
///
/// A container reaching `is_true` renders as its Python repr, which never
/// matches, so anything non-scalar is short-circuited to a sentinel.
fn scalar_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Null => "None".to_owned(),
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => String::new(),
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

    #[test]
    fn the_four_true_strings_are_recognised_whatever_their_case() {
        for text in ["true", "TRUE", "  Yes ", "on", "1"] {
            assert!(translate_bool(&json!(text)), "{text}");
        }
    }

    #[test]
    fn other_non_empty_strings_are_false_despite_being_python_truthy() {
        for text in ["no", "off", "false", "maybe", "2"] {
            assert!(py_truthy(&json!(text)), "{text}");
            assert!(!translate_bool(&json!(text)), "{text}");
        }
    }

    #[test]
    fn only_the_integer_one_is_a_true_number() {
        assert!(translate_bool(&json!(1)));
        // `str(1.0)` is "1.0", which is not in the table.
        assert!(!translate_bool(&json!(1.0)));
        assert!(!translate_bool(&json!(2)));
        assert!(!translate_bool(&json!(0)));
    }

    #[test]
    fn is_false_is_not_the_negation_of_is_true() {
        for text in ["off", "OFF", "  no ", "false", "0"] {
            assert!(is_false(&json!(text)), "{text}");
        }
        assert!(is_false(&json!(false)));
        assert!(is_false(&json!(0)));
        // Neither, which is the branch upstream then trips over. `str(0.0)` is
        // "0.0", so the value most likely to be written by hand for "off" is
        // the one that answers no to both questions.
        for value in [json!(2), json!(0.0), json!("maybe"), json!([]), json!({})] {
            assert!(!is_true(&value), "{value}");
            assert!(!is_false(&value), "{value}");
        }
    }

    #[test]
    fn a_non_empty_container_is_still_false() {
        assert!(py_truthy(&json!([1])));
        assert!(!translate_bool(&json!([1])));
        assert!(!translate_bool(&json!({"a": 1})));
    }

    #[test]
    fn a_missing_key_takes_the_default_but_an_explicit_null_does_not() {
        let cfg: Object = match json!({ "present": serde_json::Value::Null }) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert!(get_bool(&cfg, "absent", true));
        assert!(!get_bool(&cfg, "present", true));
    }
}
