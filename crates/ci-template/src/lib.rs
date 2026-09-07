//! Port of `cloudinit/templater.py`.
//!
//! cloud-init supports two template dialects, selected by a `## template: <kind>`
//! header on the first line:
//!
//! * `jinja` — Jinja2, used for `/etc/cloud/cloud.cfg.d` files and user-data that
//!   references instance metadata;
//! * `basic` — `$var` / `${a.b}` substitution, the historical default when no
//!   header is present.
//!
//! The Jinja engine is sandboxed: no filesystem or network loaders, no template
//! inheritance from disk, and a bounded output size. Templates routinely come from
//! user-data, so template injection must not become code execution or a memory
//! exhaustion vector.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use minijinja::value::{Enumerator, Object, ObjectRepr, Value as JValue, ValueKind};
use minijinja::{Environment, Output, State, UndefinedBehavior};
use serde_json::Value as Json;

/// Rendered in place of a variable the instance data does not define.
pub const MISSING_JINJA_PREFIX: &str = "CI_MISSING_JINJA_VAR/";

/// Ceiling on rendered output (16 MiB).
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    Jinja,
    Basic,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Unknown template rendering type '{0}' requested")]
    UnknownType(String),
    #[error("jinja template error: {0}")]
    Jinja(#[from] minijinja::Error),
    /// A name the basic dialect could not resolve. Upstream lets the `KeyError`
    /// escape, and its `str` is the missing key's repr and nothing else.
    #[error("'{0}'")]
    UndefinedVariable(String),
    /// A `TypeError` from traversing into something that is not a mapping.
    #[error("{0}")]
    BasicRender(String),
    #[error("rendered output exceeds {MAX_OUTPUT_BYTES} bytes")]
    OutputTooLarge,
}

impl Error {
    /// Whether the *template* failed to parse, as opposed to failing while it
    /// ran. Upstream splits the two into `JinjaSyntaxParsingException` and
    /// everything else, and callers report them differently.
    #[must_use]
    pub fn is_syntax_error(&self) -> bool {
        matches!(self, Self::Jinja(err) if err.kind() == minijinja::ErrorKind::SyntaxError)
    }
}

/// Split a template into its declared kind and its body.
///
/// Mirrors `templater.detect_template`: the header line is consumed, and a missing
/// header means `basic` over the whole text.
pub fn detect_template(text: &str) -> Result<(TemplateKind, &str), Error> {
    let (first, rest) = match text.split_once('\n') {
        Some((first, rest)) => (first, rest),
        None => (text, ""),
    };
    let Some(declared) = parse_type_header(first) else {
        return Ok((TemplateKind::Basic, text));
    };
    match declared.as_str() {
        "jinja" => Ok((TemplateKind::Jinja, rest)),
        "basic" => Ok((TemplateKind::Basic, rest)),
        other => Err(Error::UnknownType(other.to_owned())),
    }
}

/// `## template: jinja` (case-insensitive, tolerant of surrounding whitespace).
fn parse_type_header(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("##")?;
    let rest = rest.trim_start();
    let rest = rest
        .get(..9)
        .filter(|p| p.eq_ignore_ascii_case("template:"))
        .and_then(|_| rest.get(9..))?;
    Some(rest.trim().to_lowercase())
}

/// Render `content` with `params`, auto-detecting the dialect.
pub fn render_string(content: &str, params: &Json) -> Result<String, Error> {
    let (kind, body) = detect_template(content)?;
    match kind {
        TemplateKind::Jinja => render_jinja(body, params),
        TemplateKind::Basic => render_basic(body, params),
    }
}

/// Render a Jinja template against instance data.
///
/// Upstream's `jinja_render` leaves `keep_trailing_newline` at Jinja2's
/// default, which drops the template's last newline, and then appends one
/// unconditionally if the source had one. That is not the same as keeping it:
/// when the source ends in `%}` followed by a newline, `trim_blocks` eats that
/// newline and the appended one takes its place, so a template whose last line
/// is `{% endif %}` still ends in a blank line.
pub fn render_jinja(content: &str, params: &Json) -> Result<String, Error> {
    let mut env = Environment::new();
    // Match Jinja2 defaults used by cloud-init's `jinja_render`.
    env.set_trim_blocks(true);
    env.set_keep_trailing_newline(false);
    env.set_formatter(py_format);
    // Undefined names are reported inline rather than aborting the render, so a
    // template referencing an absent datasource key still produces output.
    env.set_undefined_behavior(UndefinedBehavior::Lenient);

    let tmpl = env.template_from_str(content)?;
    let mut out = tmpl.render(context_value(params))?;
    if content.ends_with('\n') {
        out.push('\n');
    }
    if out.len() > MAX_OUTPUT_BYTES {
        return Err(Error::OutputTooLarge);
    }
    Ok(out)
}

/// Render `$var` / `${a.b}` substitutions.
pub fn render_basic(content: &str, params: &Json) -> Result<String, Error> {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' || n == '.' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            out.push('$');
            if braced {
                out.push('{');
            }
            continue;
        }
        if braced {
            if chars.peek() == Some(&'}') {
                chars.next();
            } else {
                // Unterminated `${`: emit verbatim, as the regex would not match.
                out.push_str("${");
                out.push_str(&name);
                continue;
            }
        }
        out.push_str(&lookup(params, &name)?);
        if out.len() > MAX_OUTPUT_BYTES {
            return Err(Error::OutputTooLarge);
        }
    }
    Ok(out)
}

/// `basic_render`'s variable lookup, whose failures are Python's own
/// `KeyError` and `TypeError` and escape the caller with those exact texts.
fn lookup(params: &Json, dotted: &str) -> Result<String, Error> {
    let mut parts: Vec<&str> = dotted.split('.').collect();
    let Some(last) = parts.pop() else {
        return Err(Error::UndefinedVariable(dotted.to_owned()));
    };
    let mut current = params;
    for key in parts {
        let Json::Object(map) = current else {
            return Err(Error::BasicRender(format!(
                "Can not traverse into non-dictionary '{}' of type {} while \
                 looking for subkey '{key}'",
                json_str(current),
                obj_name(current)
            )));
        };
        current = map
            .get(key)
            .ok_or_else(|| Error::UndefinedVariable(key.to_owned()))?;
    }
    let Json::Object(map) = current else {
        return Err(Error::BasicRender(format!(
            "Can not extract key '{last}' from non-dictionary '{}' of type {}",
            json_str(current),
            obj_name(current)
        )));
    };
    let value = map
        .get(last)
        .ok_or_else(|| Error::UndefinedVariable(last.to_owned()))?;
    Ok(json_str(value))
}

/// `type_utils.obj_name` for a config value.
fn obj_name(value: &Json) -> &'static str {
    match value {
        Json::Null => "NoneType",
        Json::Bool(_) => "bool",
        Json::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Json::String(_) => "str",
        Json::Array(_) => "list",
        Json::Object(_) => "dict",
    }
}

/// Python's `str()`: a string is itself, anything else is its `repr`.
fn json_str(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => json_repr(other),
    }
}

fn json_repr(value: &Json) -> String {
    match value {
        Json::Null => "None".to_owned(),
        Json::Bool(true) => "True".to_owned(),
        Json::Bool(false) => "False".to_owned(),
        Json::Number(n) => n.to_string(),
        Json::String(text) => py_quote(text),
        Json::Array(items) => {
            let shown: Vec<String> = items.iter().map(json_repr).collect();
            format!("[{}]", shown.join(", "))
        }
        Json::Object(map) => {
            let shown: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", py_quote(key), json_repr(item)))
                .collect();
            format!("{{{}}}", shown.join(", "))
        }
    }
}

/// Port of `cloudinit/handlers/jinja_template.py::convert_jinja_instance_data`.
///
/// Nested `vN` namespaces are also copied to the top level, so a template can say
/// either `{{ v1.local_hostname }}` or `{{ local_hostname }}`.
pub fn convert_jinja_instance_data(data: &Json) -> Json {
    convert_jinja(data, false)
}

/// As [`convert_jinja_instance_data`], but also emitting underscore aliases for
/// keys containing jinja operators (`foo-bar` becomes reachable as `foo_bar`).
pub fn convert_jinja_instance_data_with_aliases(data: &Json) -> Json {
    convert_jinja(data, true)
}

fn convert_jinja(data: &Json, include_key_aliases: bool) -> Json {
    let Json::Object(map) = data else {
        return data.clone();
    };
    let mut sorted: Vec<_> = map.iter().collect();
    sorted.sort_by_key(|(key, _)| *key);

    let mut result = serde_json::Map::new();
    for (key, value) in sorted {
        let converted = if value.is_object() {
            convert_jinja(value, include_key_aliases)
        } else {
            value.clone()
        };
        if value.is_object() && is_version_namespace(key) {
            if let Json::Object(inner) = &converted {
                for (k, v) in inner {
                    result.insert(k.clone(), v.clone());
                }
            }
        }
        if include_key_aliases {
            if let Some(alias) = jinja_variable_alias(key) {
                result.insert(alias, converted.clone());
            }
        }
        result.insert(key.clone(), converted);
    }
    Json::Object(result)
}

/// Port of `get_jinja_variable_alias`: jinja operators become underscores.
///
/// Only `-` and `.` are substituted, matching upstream's documented fallback
/// pattern; those are the only operators that occur in real metadata keys.
pub fn jinja_variable_alias(name: &str) -> Option<String> {
    if !name.contains(['-', '.']) {
        return None;
    }
    Some(name.replace(['-', '.'], "_"))
}

/// Matches `v1`, `v2`, ... (upstream regex `v\d+$`).
fn is_version_namespace(key: &str) -> bool {
    key.strip_prefix('v').is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Build the Jinja root context, wrapping mappings so that unknown keys render as
/// `CI_MISSING_JINJA_VAR/<name>` the way cloud-init's `UndefinedJinjaVariable` does.
fn context_value(params: &Json) -> JValue {
    match params {
        Json::Object(map) => JValue::from_object(CiMap::new(map)),
        other => wrap(other),
    }
}

fn wrap(value: &Json) -> JValue {
    match value {
        Json::Null => JValue::from(()),
        Json::Bool(b) => JValue::from(*b),
        Json::Number(n) => n
            .as_i64()
            .map(JValue::from)
            .or_else(|| n.as_u64().map(JValue::from))
            .or_else(|| n.as_f64().map(JValue::from))
            .unwrap_or_else(|| JValue::from(n.to_string())),
        Json::String(s) => JValue::from(s.clone()),
        Json::Array(items) => JValue::from(items.iter().map(wrap).collect::<Vec<_>>()),
        Json::Object(map) => JValue::from_object(CiMap::new(map)),
    }
}

/// A mapping whose missing keys resolve to a [`Missing`] marker.
#[derive(Debug)]
struct CiMap {
    entries: BTreeMap<String, Json>,
    order: Vec<Arc<str>>,
}

impl CiMap {
    fn new(map: &serde_json::Map<String, Json>) -> Self {
        Self {
            entries: map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            order: map.keys().map(|k| Arc::from(k.as_str())).collect(),
        }
    }
}

impl Object for CiMap {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &JValue) -> Option<JValue> {
        let name = key.as_str()?;
        Some(match self.entries.get(name) {
            Some(value) => wrap(value),
            None => JValue::from_object(Missing(name.to_owned())),
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(self.order.iter().map(|k| JValue::from(&**k)).collect())
    }
}

/// Stands in for an undefined variable: renders as the upstream marker string and
/// is falsy, so `{% if missing %}` still takes the else branch.
#[derive(Debug)]
struct Missing(String);
impl Object for Missing {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Empty
    }

    fn get_value(self: &Arc<Self>, key: &JValue) -> Option<JValue> {
        // Attribute access on a missing value stays missing, e.g. `v1.a.b`.
        let name = key.as_str()?;
        Some(JValue::from_object(Missing(format!("{}.{name}", self.0))))
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{MISSING_JINJA_PREFIX}{}", self.0)
    }
}

/// Print a value the way Jinja2 does, which is `str()` of the underlying Python
/// object.
///
/// `MiniJinja`'s own formatting is JSON-flavoured: `none` prints as nothing,
/// booleans print lowercase, and sequences and mappings print with double
/// quotes. Templates fed a null or a list therefore rendered differently from
/// upstream until this formatter put Python's spelling back.
fn py_format(
    out: &mut Output<'_>,
    state: &State<'_, '_>,
    value: &JValue,
) -> Result<(), minijinja::Error> {
    // The undefined marker renders itself; so does anything else with a
    // hand-written `render`.
    if value.downcast_object_ref::<Missing>().is_some() {
        return minijinja::escape_formatter(out, state, value);
    }
    match py_display(value) {
        Some(text) => out.write_str(&text).map_err(minijinja::Error::from),
        None => minijinja::escape_formatter(out, state, value),
    }
}

/// `str()` of a value, for the shapes where it differs from `MiniJinja`'s
/// default.
fn py_display(value: &JValue) -> Option<String> {
    match value.kind() {
        ValueKind::None | ValueKind::Undefined if value.is_none() => {
            Some("None".to_owned())
        }
        ValueKind::Bool => {
            Some(if value.is_true() { "True" } else { "False" }.to_owned())
        }
        ValueKind::Seq | ValueKind::Map => Some(py_repr(value)),
        _ => None,
    }
}

/// `repr()` of a value, used for the insides of a sequence or mapping.
fn py_repr(value: &JValue) -> String {
    match value.kind() {
        ValueKind::None | ValueKind::Undefined => "None".to_owned(),
        ValueKind::Bool => if value.is_true() { "True" } else { "False" }.to_owned(),
        ValueKind::String => value.as_str().map_or_else(|| value.to_string(), py_quote),
        ValueKind::Seq => {
            let items: Vec<String> = value
                .try_iter()
                .map(|iter| iter.map(|item| py_repr(&item)).collect())
                .unwrap_or_default();
            format!("[{}]", items.join(", "))
        }
        ValueKind::Map => {
            let Ok(iter) = value.try_iter() else {
                return "{}".to_owned();
            };
            let items: Vec<String> = iter
                .map(|key| {
                    let shown = py_repr(&key);
                    let held =
                        value.get_item(&key).unwrap_or_else(|_| JValue::from(()));
                    format!("{shown}: {}", py_repr(&held))
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        _ => value.to_string(),
    }
}

/// Python prefers single quotes, switching to double only to avoid escaping.
fn py_quote(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
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

    fn params() -> Json {
        json!({
            "v1": {"local_hostname": "ubuntu", "cloud_name": "azure"},
            "ds": {"meta_data": {"instance_id": "i-1234"}},
        })
    }

    #[test]
    fn detects_headers() {
        let (kind, body) = detect_template("## template: jinja\nhi\n").unwrap();
        assert_eq!(kind, TemplateKind::Jinja);
        assert_eq!(body, "hi\n");

        let (kind, body) = detect_template("##template:BASIC\nhi\n").unwrap();
        assert_eq!(kind, TemplateKind::Basic);
        assert_eq!(body, "hi\n");

        let (kind, body) = detect_template("plain $text\n").unwrap();
        assert_eq!(kind, TemplateKind::Basic);
        assert_eq!(body, "plain $text\n");

        assert!(detect_template("## template: erb\nx").is_err());
    }

    #[test]
    fn renders_jinja_variables() {
        let out = render_string(
            "## template: jinja\nhost={{ v1.local_hostname }}\n",
            &params(),
        )
        .unwrap();
        assert_eq!(out, "host=ubuntu\n");
    }

    #[test]
    fn missing_jinja_variables_use_the_upstream_marker() {
        let out =
            render_string("## template: jinja\n{{ v1.nope }}\n", &params()).unwrap();
        assert_eq!(out, "CI_MISSING_JINJA_VAR/nope\n");
    }

    #[test]
    fn missing_jinja_variables_are_falsy() {
        let out = render_string(
            "## template: jinja\n{% if v1.nope %}yes{% else %}no{% endif %}\n",
            &params(),
        )
        .unwrap();
        assert_eq!(out, "no\n");
    }

    #[test]
    fn renders_basic_substitutions() {
        let out = render_basic(
            "id=${ds.meta_data.instance_id} name=$v1.cloud_name\n",
            &params(),
        )
        .unwrap();
        assert_eq!(out, "id=i-1234 name=azure\n");
    }

    #[test]
    fn basic_render_fails_on_unknown_names() {
        assert!(matches!(
            render_basic("$nope", &params()),
            Err(Error::UndefinedVariable(_))
        ));
    }

    #[test]
    fn jinja_has_no_filesystem_access() {
        // No loader is configured, so include/extends cannot reach the disk.
        let err =
            render_string("## template: jinja\n{% include '/etc/shadow' %}", &params())
                .unwrap_err();
        assert!(matches!(err, Error::Jinja(_)), "{err}");
    }
}
