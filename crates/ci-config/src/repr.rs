//! Python's `repr()` over a config value, for log lines that quote what the
//! operator wrote.

use crate::Value;

/// `repr()` of a config value, far enough for the messages the port emits.
#[must_use]
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => repr_str(text),
        Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(repr).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_owned();
            }
            let rendered: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{}: {}", repr_str(key), repr(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

/// Python prefers single quotes, switching to double only to avoid escaping.
#[must_use]
pub fn repr_str(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|character| match character {
            '\\' => "\\\\".to_owned(),
            '\n' => "\\n".to_owned(),
            '\r' => "\\r".to_owned(),
            '\t' => "\\t".to_owned(),
            other => other.to_string(),
        })
        .collect();
    if text.contains('\'') && !text.contains('"') {
        format!("\"{escaped}\"")
    } else {
        format!("'{}'", escaped.replace('\'', "\\'"))
    }
}

/// `type_utils.obj_name`: the name upstream puts in a "unknown type %s"
/// message.
#[must_use]
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) => {
            if number.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
