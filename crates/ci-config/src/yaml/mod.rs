//! YAML loading with explicit resource limits.
//!
//! Untrusted YAML reaches cloud-init from seed media, IMDS and user-data. A plain
//! `safe_load` is not enough: anchor/alias expansion ("billion laughs") can turn a
//! few kilobytes into gigabytes *inside the parser*, before any post-parse check
//! could run. Aliases are therefore counted in the raw text and rejected up front,
//! and the parsed tree is bounded for depth and node count afterwards.

use crate::{Object, Value};

mod loader;
mod resolve;

pub use loader::ParseError;

/// Resource ceilings applied to a single YAML document.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    /// Maximum number of `*alias` references in the raw document.
    pub max_aliases: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024 * 1024,
            max_depth: 64,
            max_nodes: 500_000,
            max_aliases: 256,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum YamlError {
    #[error("document is {actual} bytes, exceeding the {limit} byte limit")]
    TooLarge { actual: usize, limit: usize },
    #[error("document uses {actual} aliases, exceeding the {limit} alias limit")]
    TooManyAliases { actual: usize, limit: usize },
    #[error("document nests deeper than {limit} levels")]
    TooDeep { limit: usize },
    #[error("document has more than {limit} nodes")]
    TooManyNodes { limit: usize },
    #[error("invalid YAML: {0}")]
    Parse(#[from] ParseError),
    #[error("expected a mapping at the top level, found {found}")]
    NotAMapping { found: &'static str },
}

/// Parse a YAML document into the canonical [`Value`] representation.
pub fn load_yaml(text: &str, limits: Limits) -> Result<Value, YamlError> {
    if text.len() > limits.max_bytes {
        return Err(YamlError::TooLarge {
            actual: text.len(),
            limit: limits.max_bytes,
        });
    }
    let aliases = count_aliases(text);
    if aliases > limits.max_aliases {
        return Err(YamlError::TooManyAliases {
            actual: aliases,
            limit: limits.max_aliases,
        });
    }

    let value: Value = loader::load(text)?;
    check_shape(&value, limits)?;
    Ok(value)
}

/// Parse a cloud-config document, which must be a mapping.
///
/// An empty or comment-only document yields an empty mapping, matching
/// `util.load_yaml(blob, default={})`.
pub fn load_mapping(text: &str, limits: Limits) -> Result<Object, YamlError> {
    match load_yaml(text, limits)? {
        Value::Null => Ok(Object::new()),
        Value::Object(map) => Ok(map),
        other => Err(YamlError::NotAMapping {
            found: type_name(&other),
        }),
    }
}

pub(crate) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "mapping",
    }
}

/// Count `*alias` references outside of quoted scalars and comments.
///
/// Deliberately conservative: over-counting only makes the limit stricter.
fn count_aliases(text: &str) -> usize {
    let mut count = 0usize;
    let mut prev = '\n';
    let mut in_single = false;
    let mut in_double = false;
    let mut in_comment = false;

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
            prev = c;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && prev.is_whitespace() => {
                in_comment = true;
            }
            '*' if !in_single && !in_double => {
                let starts_token =
                    prev.is_whitespace() || matches!(prev, '[' | '{' | ',' | '-' | ':');
                let named = chars
                    .peek()
                    .is_some_and(|n| n.is_alphanumeric() || *n == '_');
                if starts_token && named {
                    count += 1;
                }
            }
            _ => {}
        }
        prev = c;
    }
    count
}

fn check_shape(value: &Value, limits: Limits) -> Result<(), YamlError> {
    let mut nodes = 0usize;
    walk(value, 1, limits, &mut nodes)
}

fn walk(
    value: &Value,
    depth: usize,
    limits: Limits,
    nodes: &mut usize,
) -> Result<(), YamlError> {
    if depth > limits.max_depth {
        return Err(YamlError::TooDeep {
            limit: limits.max_depth,
        });
    }
    *nodes += 1;
    if *nodes > limits.max_nodes {
        return Err(YamlError::TooManyNodes {
            limit: limits.max_nodes,
        });
    }
    match value {
        Value::Array(items) => {
            for item in items {
                walk(item, depth + 1, limits, nodes)?;
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                walk(item, depth + 1, limits, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
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
    fn loads_a_mapping_and_preserves_order() {
        let cfg = load_mapping("zeta: 1\nalpha: 2\n", Limits::default()).unwrap();
        let keys: Vec<_> = cfg.keys().cloned().collect();
        assert_eq!(keys, vec!["zeta".to_owned(), "alpha".to_owned()]);
    }

    #[test]
    fn empty_document_is_an_empty_mapping() {
        let cfg = load_mapping("# just a comment\n", Limits::default()).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn rejects_non_mapping_top_level() {
        assert!(load_mapping("- a\n- b\n", Limits::default()).is_err());
    }

    #[test]
    fn rejects_alias_bombs() {
        let bomb = "\
a: &a [\"x\",\"x\"]
b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]
c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]
";
        let limits = Limits {
            max_aliases: 4,
            ..Limits::default()
        };
        assert!(matches!(
            load_yaml(bomb, limits),
            Err(YamlError::TooManyAliases { .. })
        ));
    }

    #[test]
    fn asterisks_in_strings_are_not_aliases() {
        let text = "runcmd:\n  - \"chmod a+rwx *\"\n  - 'glob *.cfg'\n";
        assert_eq!(count_aliases(text), 0);
    }

    #[test]
    fn rejects_excessive_nesting() {
        let deep = format!("{}b: 1{}", "a: {".repeat(80), "}".repeat(80));
        assert!(matches!(
            load_yaml(&deep, Limits::default()),
            Err(YamlError::TooDeep { .. })
        ));
    }

    #[test]
    fn quoting_suppresses_the_implicit_resolver() {
        let cfg =
            load_mapping("a: yes\nb: 'yes'\nc: \"0600\"\n", Limits::default()).unwrap();
        assert_eq!(cfg["a"], Value::Bool(true));
        assert_eq!(cfg["b"], Value::String("yes".to_owned()));
        assert_eq!(cfg["c"], Value::String("0600".to_owned()));
    }

    #[test]
    fn block_scalars_are_always_strings() {
        let cfg = load_mapping("a: |\n  yes\n", Limits::default()).unwrap();
        assert_eq!(cfg["a"], Value::String("yes\n".to_owned()));
    }

    #[test]
    fn merges_inherited_keys_without_overriding_explicit_ones() {
        let cfg = load_mapping(
            "base: &b\n  owner: root\n  mode: '0600'\nfile:\n  <<: *b\n  owner: nobody\n",
            Limits::default(),
        )
        .unwrap();
        let file = cfg["file"].as_object().unwrap();
        assert_eq!(file["owner"], Value::String("nobody".to_owned()));
        assert_eq!(file["mode"], Value::String("0600".to_owned()));
    }

    #[test]
    fn the_first_mapping_in_a_merge_list_wins() {
        let cfg = load_mapping(
            "a: &a {k: 1}\nb: &b {k: 2}\nc:\n  <<: [*a, *b]\n",
            Limits::default(),
        )
        .unwrap();
        assert_eq!(cfg["c"].as_object().unwrap()["k"], Value::from(1));
    }

    #[test]
    fn aliases_expand_to_the_anchored_value() {
        let cfg = load_mapping("a: &x [1, 2]\nb: *x\n", Limits::default()).unwrap();
        assert_eq!(cfg["a"], cfg["b"]);
    }

    #[test]
    fn rejects_an_undefined_alias() {
        assert!(load_yaml("a: *nope\n", Limits::default()).is_err());
    }

    #[test]
    fn rejects_more_than_one_document() {
        assert!(load_yaml("a: 1\n---\nb: 2\n", Limits::default()).is_err());
    }

    #[test]
    fn a_parse_error_carries_a_one_based_position() {
        let Err(YamlError::Parse(error)) =
            load_yaml(": : :\n  - [\n", Limits::default())
        else {
            panic!("expected a parse error");
        };
        assert_eq!(error.line(), 2);
        assert_eq!(error.column(), 5);
    }
}
