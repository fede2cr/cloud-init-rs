//! Builds a [`Value`] from the parser's event stream.
//!
//! The tree is assembled from events rather than deserialised so that each
//! scalar's *style* is visible: only plain scalars go through `PyYAML`'s
//! implicit resolver, while quoted and block scalars stay strings. A
//! deserialising front end discards that distinction and cannot tell `yes` from
//! `"yes"`.

use std::collections::HashMap;

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag as YamlTag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

use super::resolve::{self, Tag};
use crate::{Object, Value};

/// A YAML syntax or construction error, with the position that provoked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
    line: usize,
    column: usize,
}

impl ParseError {
    /// 1-based line, matching `PyYAML`'s marks.
    #[must_use]
    pub fn line(&self) -> usize {
        self.line
    }

    /// 1-based column, matching `PyYAML`'s marks.
    #[must_use]
    pub fn column(&self) -> usize {
        self.column
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at line {}, column {}",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for ParseError {}

/// A node that has been parsed but not yet attached to its parent.
#[derive(Debug, Clone)]
enum Node {
    Scalar(Value),
    /// A plain `<<`, which only has meaning as a mapping key.
    Merge,
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Debug)]
enum Frame {
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>, Option<Node>),
}

#[derive(Debug, Default)]
struct Builder {
    stack: Vec<Frame>,
    /// Anchors on collections arrive with the start event but can only be
    /// recorded once the value exists, so they wait here until the end event.
    collection_anchors: Vec<usize>,
    anchors: HashMap<usize, Node>,
    root: Option<Node>,
    error: Option<ParseError>,
    documents: usize,
}

impl Builder {
    fn fail(&mut self, message: impl Into<String>, mark: Marker) {
        if self.error.is_none() {
            self.error = Some(ParseError {
                message: message.into(),
                line: mark.line(),
                column: mark.col() + 1,
            });
        }
    }

    fn push(&mut self, node: Node, anchor: usize) {
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            Some(Frame::Seq(items)) => items.push(node),
            Some(Frame::Map(pairs, pending)) => match pending.take() {
                Some(key) => pairs.push((key, node)),
                None => *pending = Some(node),
            },
            None => self.root = Some(node),
        }
    }

    fn close(&mut self, anchor: usize) {
        let node = match self.stack.pop() {
            Some(Frame::Seq(items)) => Node::Seq(items),
            Some(Frame::Map(pairs, _)) => Node::Map(pairs),
            None => return,
        };
        self.push(node, anchor);
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, mark: Marker) {
        if self.error.is_some() {
            return;
        }
        match event {
            Event::DocumentStart => {
                self.documents += 1;
                if self.documents > 1 {
                    self.fail("expected a single document in the stream", mark);
                }
            }
            Event::Scalar(text, style, anchor, tag) => {
                match scalar(&text, style, tag.as_ref()) {
                    Ok(node) => self.push(node, anchor),
                    Err(message) => self.fail(message, mark),
                }
            }
            Event::SequenceStart(anchor, _) => {
                self.stack.push(Frame::Seq(Vec::new()));
                self.collection_anchors.push(anchor);
            }
            Event::MappingStart(anchor, _) => {
                self.stack.push(Frame::Map(Vec::new(), None));
                self.collection_anchors.push(anchor);
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let anchor = self.collection_anchors.pop().unwrap_or(0);
                self.close(anchor);
            }
            Event::Alias(anchor) => match self.anchors.get(&anchor).cloned() {
                Some(node) => self.push(node, 0),
                None => self.fail(format!("found undefined alias {anchor}"), mark),
            },
            _ => {}
        }
    }
}

fn scalar(
    text: &str,
    style: TScalarStyle,
    tag: Option<&YamlTag>,
) -> Result<Node, String> {
    if let Some(tag) = tag {
        return tagged(text, tag);
    }
    if style != TScalarStyle::Plain {
        return Ok(Node::Scalar(Value::String(text.to_owned())));
    }
    Ok(match resolve::resolve(text) {
        Tag::Null => Node::Scalar(Value::Null),
        Tag::Bool => Node::Scalar(Value::Bool(resolve::bool_value(text))),
        Tag::Int => Node::Scalar(number(resolve::int_value(text), text)),
        Tag::Float => Node::Scalar(number(resolve::float_value(text), text)),
        Tag::Merge => Node::Merge,
        // SafeLoader has no constructor for `tag:yaml.org,2002:value`.
        Tag::Value => return Err(
            "could not determine a constructor for the tag 'tag:yaml.org,2002:value'"
                .to_owned(),
        ),
        // JSON has no date type; see docs/COMPAT.md.
        Tag::Timestamp | Tag::Str => Node::Scalar(Value::String(text.to_owned())),
    })
}

/// Values outside JSON's numeric range fall back to the source text.
fn number(parsed: Option<Value>, text: &str) -> Value {
    parsed.unwrap_or_else(|| Value::String(text.to_owned()))
}

fn tagged(text: &str, tag: &YamlTag) -> Result<Node, String> {
    if tag.handle != "tag:yaml.org,2002:" {
        return Ok(Node::Scalar(Value::String(text.to_owned())));
    }
    let value = match tag.suffix.as_str() {
        "null" => Value::Null,
        "bool" => {
            if resolve::resolve(text) == Tag::Bool {
                Value::Bool(resolve::bool_value(text))
            } else {
                return Err(format!(
                    "could not determine a constructor for the tag {text:?}"
                ));
            }
        }
        "int" => resolve::int_value(text)
            .ok_or_else(|| format!("failed to construct an int from {text:?}"))?,
        "float" => resolve::float_value(text)
            .ok_or_else(|| format!("failed to construct a float from {text:?}"))?,
        // `str` and anything the port does not model keep the source text.
        _ => Value::String(text.to_owned()),
    };
    Ok(Node::Scalar(value))
}

/// Convert the node tree, applying `PyYAML`'s `flatten_mapping` for `<<`.
fn build(node: Node) -> Result<Value, String> {
    Ok(match node {
        Node::Scalar(value) => value,
        Node::Merge => Value::String("<<".to_owned()),
        Node::Seq(items) => Value::Array(
            items
                .into_iter()
                .map(build)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Node::Map(pairs) => {
            let mut merged: Vec<(Node, Node)> = Vec::new();
            let mut own: Vec<(Node, Node)> = Vec::new();
            for (key, value) in pairs {
                if matches!(key, Node::Merge) {
                    merged.extend(merge_source(value)?);
                } else {
                    own.push((key, value));
                }
            }
            // `node.value = merge + node.value`: merged keys come first, so a
            // key written out explicitly overwrites the one it inherited.
            let mut map = Object::new();
            for (key, value) in merged.into_iter().chain(own) {
                map.insert(key_string(&build(key)?), build(value)?);
            }
            Value::Object(map)
        }
    })
}

/// The pairs a `<<` contributes: one mapping, or a list of them where the
/// earliest entry wins.
fn merge_source(value: Node) -> Result<Vec<(Node, Node)>, String> {
    match value {
        Node::Map(pairs) => Ok(pairs),
        Node::Seq(items) => {
            let mut out = Vec::new();
            for item in items.into_iter().rev() {
                match item {
                    Node::Map(pairs) => out.extend(pairs),
                    _ => return Err("expected a mapping for merging".to_owned()),
                }
            }
            Ok(out)
        }
        _ => Err("expected a mapping or list of mappings for merging".to_owned()),
    }
}

/// JSON object keys are strings; render a non-string key as it was written.
fn key_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Parse a single YAML document.
pub(super) fn load(text: &str) -> Result<Value, ParseError> {
    let mut builder = Builder::default();
    let mut parser = Parser::new_from_str(text);
    if let Err(scan) = parser.load(&mut builder, true) {
        let mark = scan.marker();
        return Err(ParseError {
            message: scan.info().to_owned(),
            line: mark.line(),
            column: mark.col() + 1,
        });
    }
    if let Some(error) = builder.error {
        return Err(error);
    }
    let Some(root) = builder.root else {
        return Ok(Value::Null);
    };
    build(root).map_err(|message| ParseError {
        message,
        line: 1,
        column: 1,
    })
}
