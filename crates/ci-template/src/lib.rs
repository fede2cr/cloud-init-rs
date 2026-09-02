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

use minijinja::value::{Enumerator, Object, ObjectRepr, Value as JValue};
use minijinja::{Environment, UndefinedBehavior};
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
    #[error("undefined variable '{0}' in basic template")]
    UndefinedVariable(String),
    #[error("rendered output exceeds {MAX_OUTPUT_BYTES} bytes")]
    OutputTooLarge,
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
pub fn render_jinja(content: &str, params: &Json) -> Result<String, Error> {
    let mut env = Environment::new();
    // Match Jinja2 defaults used by cloud-init's `jinja_render`.
    env.set_trim_blocks(true);
    env.set_keep_trailing_newline(true);
    // Undefined names are reported inline rather than aborting the render, so a
    // template referencing an absent datasource key still produces output.
    env.set_undefined_behavior(UndefinedBehavior::Lenient);

    let tmpl = env.template_from_str(content)?;
    let out = tmpl.render(context_value(params))?;
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

fn lookup(params: &Json, dotted: &str) -> Result<String, Error> {
    let mut current = params;
    for segment in dotted.split('.') {
        current = current
            .get(segment)
            .ok_or_else(|| Error::UndefinedVariable(dotted.to_owned()))?;
    }
    Ok(match current {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    })
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
        assert_eq!(out, "no");
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
