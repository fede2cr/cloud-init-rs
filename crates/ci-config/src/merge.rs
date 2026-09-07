//! Port of `cloudinit/mergers/*`.
//!
//! The upstream model is a `LookupMerger` holding an ordered set of per-type
//! mergers. Merging dispatches on the type of the *source* value; the first merger
//! that handles that type wins, and if none does, the source value is kept. That
//! last rule is why the default `list()+dict()+str()` behaves as "first config
//! wins" rather than "last wins", which is the single most surprising piece of
//! cloud-init semantics.

use crate::{Object, Value};

/// `cloudinit.mergers.DEF_MERGE_TYPE`.
pub const DEFAULT_MERGE_TYPE: &str = "list()+dict()+str()";

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("Matcher identifier '{0}' is not in the right format")]
    BadFormat(String),
    #[error("Could not find merger module named '{0}'")]
    UnknownMerger(String),
    #[error("invalid merge_how entry: {0}")]
    BadMergeHow(String),
}

/// One parsed `name(opt,opt)` entry from a `merge_how` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergerSpec {
    pub name: String,
    pub opts: Vec<String>,
}

impl MergerSpec {
    fn has(&self, opt: &str) -> bool {
        self.opts.iter().any(|o| o == opt)
    }

    /// First of `candidates` present in `opts`, else `default`.
    fn pick<'a>(&self, candidates: &[&'a str], default: &'a str) -> &'a str {
        candidates
            .iter()
            .copied()
            .find(|c| self.has(c))
            .unwrap_or(default)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    List,
    Dict,
    Str,
}

/// An ordered set of type mergers, equivalent to `LookupMerger`.
#[derive(Debug, Clone)]
pub struct Mergers {
    entries: Vec<(Kind, MergerSpec)>,
}

impl Default for Mergers {
    fn default() -> Self {
        Self::default_mergers()
    }
}

impl Mergers {
    /// `mergers.default_mergers()`.
    pub fn default_mergers() -> Self {
        // The default string is a compile-time constant and always parses.
        Self::construct(&string_extract_mergers(DEFAULT_MERGE_TYPE).unwrap_or_default())
            .unwrap_or(Self {
                entries: Vec::new(),
            })
    }

    /// `mergers.construct()`.
    pub fn construct(specs: &[MergerSpec]) -> Result<Self, MergeError> {
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            let name = spec.name.strip_prefix("m_").unwrap_or(&spec.name);
            let kind = match name {
                "list" => Kind::List,
                "dict" => Kind::Dict,
                "str" => Kind::Str,
                other => return Err(MergeError::UnknownMerger(other.to_owned())),
            };
            entries.push((kind, spec.clone()));
        }
        Ok(Self { entries })
    }

    /// Merge `merge_with` into `source`, dispatching on the type of `source`.
    pub fn merge(&self, source: Value, merge_with: Value) -> Value {
        let Some((kind, spec)) = self.lookup(&source) else {
            // `UnknownMerger._handle_unknown` keeps the source value.
            return source;
        };
        match kind {
            Kind::Dict => self.merge_dict(spec, source, merge_with),
            Kind::List => self.merge_list(spec, source, merge_with),
            Kind::Str => Self::merge_str(spec, source, merge_with),
        }
    }

    /// Merge two mappings, the common entry point for cloud-config.
    pub fn merge_objects(&self, source: Object, merge_with: Object) -> Object {
        match self.merge(Value::Object(source), Value::Object(merge_with)) {
            Value::Object(map) => map,
            _ => Object::new(),
        }
    }

    fn lookup(&self, source: &Value) -> Option<(Kind, &MergerSpec)> {
        let wanted = match source {
            Value::Object(_) => Kind::Dict,
            Value::Array(_) => Kind::List,
            Value::String(_) => Kind::Str,
            _ => return None,
        };
        self.entries
            .iter()
            .find(|(kind, _)| *kind == wanted)
            .map(|(kind, spec)| (*kind, spec))
    }

    /// `m_dict.Merger._on_dict`.
    fn merge_dict(&self, spec: &MergerSpec, source: Value, merge_with: Value) -> Value {
        let Value::Object(incoming) = merge_with else {
            // Upstream returns the source untouched when merge_with is not a dict.
            return source;
        };
        let Value::Object(mut merged) = source else {
            return Value::Object(incoming);
        };
        let do_replace =
            spec.pick(&["replace", "no_replace"], "no_replace") == "replace";
        let allow_delete = spec.has("allow_delete");
        let recurse_str = spec.has("recurse_str");
        let recurse_array = spec.has("recurse_array") || spec.has("recurse_list");

        for (key, new_v) in incoming {
            // Take the old value out in place so that key order is preserved;
            // remove/insert would move the key to the end of the mapping.
            let existing = merged
                .get_mut(&key)
                .map(|slot| std::mem::replace(slot, Value::Null));
            let Some(old_v) = existing else {
                merged.insert(key, new_v);
                continue;
            };
            if new_v.is_null() && allow_delete {
                merged.remove(&key);
                continue;
            }
            let value = if do_replace {
                new_v
            } else if (new_v.is_array() && recurse_array)
                || (new_v.is_string() && recurse_str)
                // `_recurse_dict` is unconditionally on for backwards compat.
                || new_v.is_object()
            {
                self.merge(old_v, new_v)
            } else {
                old_v
            };
            if let Some(slot) = merged.get_mut(&key) {
                *slot = value;
            }
        }
        Value::Object(merged)
    }

    /// `m_list.Merger._on_list`.
    fn merge_list(&self, spec: &MergerSpec, source: Value, merge_with: Value) -> Value {
        let method =
            spec.pick(&["append", "prepend", "replace", "no_replace"], "replace");
        if method == "replace" && !merge_with.is_array() {
            return merge_with;
        }
        let Value::Array(items) = source else {
            return merge_with;
        };
        // Upstream relies on Python iterating a non-list; treat a scalar as a
        // single element instead of exploding a string into characters.
        let incoming = match merge_with {
            Value::Array(v) => v,
            other => vec![other],
        };

        match method {
            "prepend" => {
                let mut out = incoming;
                out.extend(items);
                Value::Array(out)
            }
            "append" => {
                let mut out = items;
                out.extend(incoming);
                Value::Array(out)
            }
            _ => {
                let keep_old = method == "no_replace";
                let recurse_str = spec.has("recurse_str");
                let recurse_dict = spec.has("recurse_dict");
                let recurse_array =
                    spec.has("recurse_array") || spec.has("recurse_list");
                let mut out = items;
                for (i, new_v) in incoming.into_iter().enumerate() {
                    let Some(slot) = out.get_mut(i) else { break };
                    if keep_old {
                        continue;
                    }
                    let old_v = std::mem::replace(slot, Value::Null);
                    let merged = if (new_v.is_array() && recurse_array)
                        || (new_v.is_string() && recurse_str)
                        || (new_v.is_object() && recurse_dict)
                    {
                        self.merge(old_v, new_v)
                    } else {
                        new_v
                    };
                    if let Some(slot) = out.get_mut(i) {
                        *slot = merged;
                    }
                }
                Value::Array(out)
            }
        }
    }

    /// `m_str.Merger._on_str`.
    fn merge_str(spec: &MergerSpec, source: Value, merge_with: Value) -> Value {
        if !spec.has("append") {
            return merge_with;
        }
        match (source, merge_with) {
            (Value::String(a), Value::String(b)) => Value::String(a + &b),
            (_, other) => other,
        }
    }
}

/// `mergers.string_extract_mergers()`.
pub fn string_extract_mergers(merge_how: &str) -> Result<Vec<MergerSpec>, MergeError> {
    let mut parsed = Vec::new();
    for raw in merge_how.split('+') {
        let name = canonicalize(raw);
        if name.is_empty() {
            continue;
        }
        let Some((head, rest)) = name.split_once('(') else {
            return Err(MergeError::BadFormat(name));
        };
        let Some(opts) = rest.strip_suffix(')') else {
            return Err(MergeError::BadFormat(name));
        };
        if !is_identifier(head) {
            return Err(MergeError::BadFormat(name));
        }
        parsed.push(MergerSpec {
            name: head.to_owned(),
            opts: opts
                .split(',')
                .map(|o| o.trim().to_lowercase())
                .filter(|o| !o.is_empty())
                .collect(),
        });
    }
    Ok(parsed)
}

/// `mergers.dict_extract_mergers()` — reads and removes `merge_how`/`merge_type`.
pub fn take_mergers(config: &mut Object) -> Result<Vec<MergerSpec>, MergeError> {
    let raw = config
        .remove("merge_how")
        .or_else(|| config.remove("merge_type"));
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    match raw {
        Value::String(s) => string_extract_mergers(&s),
        Value::Array(items) => items.into_iter().map(parse_merger_entry).collect(),
        other => Err(MergeError::BadMergeHow(
            crate::yaml::type_name(&other).into(),
        )),
    }
}

fn parse_merger_entry(entry: Value) -> Result<MergerSpec, MergeError> {
    match entry {
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| MergeError::BadMergeHow("missing 'name'".to_owned()))?;
            let opts = map.get("settings").map(collect_opts).unwrap_or_default();
            Ok(MergerSpec {
                name: canonicalize(name),
                opts,
            })
        }
        Value::Array(items) => {
            let mut iter = items.into_iter();
            let name = iter
                .next()
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or_else(|| {
                    MergeError::BadMergeHow("missing merger name".to_owned())
                })?;
            let opts = iter
                .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
                .filter(|s| !s.is_empty())
                .collect();
            Ok(MergerSpec {
                name: canonicalize(&name),
                opts,
            })
        }
        other => Err(MergeError::BadMergeHow(
            crate::yaml::type_name(&other).into(),
        )),
    }
}

fn collect_opts(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => s
            .split(',')
            .map(|o| o.trim().to_lowercase())
            .filter(|o| !o.is_empty())
            .collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn canonicalize(name: &str) -> String {
    name.trim().to_lowercase().replace('-', "_")
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `util.mergemanydict()`.
///
/// With the default mergers this is "first source wins"; pass `reverse` to give
/// the *last* source the highest priority, as `fetch_base_config` does.
pub fn merge_many(sources: Vec<Object>, reverse: bool) -> Object {
    let mergers = Mergers::default_mergers();
    let iter: Box<dyn Iterator<Item = Object>> = if reverse {
        Box::new(sources.into_iter().rev())
    } else {
        Box::new(sources.into_iter())
    };
    let mut merged = Object::new();
    for cfg in iter {
        if cfg.is_empty() {
            continue;
        }
        merged = mergers.merge_objects(merged, cfg);
    }
    merged
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
    use crate::yaml::{load_mapping, Limits};

    fn cfg(text: &str) -> Object {
        load_mapping(text, Limits::default()).unwrap()
    }

    fn merge(how: &str, a: &str, b: &str) -> Object {
        let specs = string_extract_mergers(how).unwrap();
        Mergers::construct(&specs)
            .unwrap()
            .merge_objects(cfg(a), cfg(b))
    }

    #[test]
    fn parses_merge_how_strings() {
        let specs =
            string_extract_mergers("dict(recurse_array,no_replace)+list(append)")
                .unwrap();
        assert_eq!(
            specs,
            vec![
                MergerSpec {
                    name: "dict".into(),
                    opts: vec!["recurse_array".into(), "no_replace".into()],
                },
                MergerSpec {
                    name: "list".into(),
                    opts: vec!["append".into()],
                },
            ]
        );
    }

    #[test]
    fn rejects_malformed_merge_how() {
        assert!(string_extract_mergers("dict").is_err());
        assert!(string_extract_mergers("9dict()").is_err());
    }

    #[test]
    fn default_merge_keeps_the_first_value() {
        let merged = Mergers::default_mergers().merge_objects(
            cfg("runcmd: [a]\nhostname: first\n"),
            cfg("hostname: second\n"),
        );
        assert_eq!(merged["hostname"], Value::String("first".into()));
    }

    #[test]
    fn default_merge_adds_new_keys_and_recurses_into_dicts() {
        let merged = Mergers::default_mergers().merge_objects(
            cfg("apt:\n  primary: one\n"),
            cfg("apt:\n  security: two\nlocale: en_US\n"),
        );
        assert_eq!(merged["apt"]["primary"], Value::String("one".into()));
        assert_eq!(merged["apt"]["security"], Value::String("two".into()));
        assert_eq!(merged["locale"], Value::String("en_US".into()));
    }

    #[test]
    fn dict_replace_overwrites() {
        let merged = merge("dict(replace)+list()+str()", "a: 1\n", "a: 2\n");
        assert_eq!(merged["a"], Value::from(2));
    }

    #[test]
    fn dict_allow_delete_removes_keys() {
        let merged = merge(
            "dict(allow_delete)+list()+str()",
            "a: 1\nb: 2\n",
            "a: null\n",
        );
        assert!(!merged.contains_key("a"));
        assert_eq!(merged["b"], Value::from(2));
    }

    #[test]
    fn list_append_and_prepend() {
        let appended = merge(
            "dict(recurse_array)+list(append)",
            "runcmd: [one]\n",
            "runcmd: [two]\n",
        );
        assert_eq!(appended["runcmd"], serde_json::json!(["one", "two"]));

        let prepended = merge(
            "dict(recurse_array)+list(prepend)",
            "runcmd: [one]\n",
            "runcmd: [two]\n",
        );
        assert_eq!(prepended["runcmd"], serde_json::json!(["two", "one"]));
    }

    #[test]
    fn str_append_concatenates() {
        let merged = merge(
            "dict(recurse_str)+str(append)",
            "message: hello\n",
            "message: ' world'\n",
        );
        assert_eq!(merged["message"], Value::String("hello world".into()));
    }

    #[test]
    fn scalars_keep_the_source_value() {
        // No merger handles numbers, so `_handle_unknown` returns the source.
        let merged = Mergers::default_mergers().merge(Value::from(1), Value::from(2));
        assert_eq!(merged, Value::from(1));
    }

    #[test]
    fn merge_many_reverse_gives_the_last_source_priority() {
        let merged = merge_many(
            vec![
                cfg("hostname: builtin\n"),
                cfg("hostname: confd\n"),
                cfg("hostname: cmdline\n"),
            ],
            true,
        );
        assert_eq!(merged["hostname"], Value::String("cmdline".into()));
    }

    #[test]
    fn take_mergers_consumes_the_key() {
        let mut config = cfg("merge_how: 'dict(replace)+list()'\nfoo: 1\n");
        let specs = take_mergers(&mut config).unwrap();
        assert_eq!(specs.len(), 2);
        assert!(!config.contains_key("merge_how"));
    }
}
