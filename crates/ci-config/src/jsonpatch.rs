//! Port of `jsonpatch` 1.32 and `jsonpointer` 2.4 — the pair upstream applies
//! `#cloud-config-jsonp` parts with.
//!
//! RFC 6902 and RFC 6901 define the formats, but this follows the two libraries
//! rather than the RFCs, because parity is with what cloud-init actually does.
//! Where they disagree the divergence is marked at the site.
//!
//! Error messages are not reproduced exactly. Upstream only logs them; nothing
//! about a failed patch reaches disk except *that* it failed, and — for the one
//! class Python calls `ValueError` — which list the part is recorded in.

use std::collections::HashSet;
use std::fmt;

use serde::de::{DeserializeSeed as _, MapAccess, SeqAccess, Visitor};

use crate::{Object, Value};

/// A patch that could not be parsed or could not be applied.
///
/// Upstream raises six exception types here, but `CloudConfigPartHandler` only
/// separates `ValueError` from everything else — a part that fails with one is
/// recorded in the written `cloud-config.txt`, a part that fails with any other
/// is not recorded at all. That is the only distinction worth keeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// Python's `ValueError`: malformed JSON, or a sequence index that
    /// `jsonpointer`'s regex accepted and `int()` then rejected.
    Value(String),
    /// `InvalidJsonPatch`, `JsonPatchConflict`, `JsonPointerException`,
    /// `JsonPatchTestFailed` and `TypeError`.
    Failed(String),
}

impl PatchError {
    /// Whether Python would have raised a `ValueError`.
    pub fn is_value_error(&self) -> bool {
        matches!(self, Self::Value(_))
    }

    fn message(&self) -> &str {
        match self {
            Self::Value(m) | Self::Failed(m) => m,
        }
    }
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PatchError {}

/// `type(x)` as Python prints it, for the messages that embed it.
fn py_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "<class 'NoneType'>",
        Value::Bool(_) => "<class 'bool'>",
        Value::Number(n) if n.is_f64() => "<class 'float'>",
        Value::Number(_) => "<class 'int'>",
        Value::String(_) => "<class 'str'>",
        Value::Array(_) => "<class 'list'>",
        Value::Object(_) => "<class 'dict'>",
    }
}

/// `jsonpointer.JsonPointer`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pointer {
    parts: Vec<String>,
}

impl Pointer {
    fn parse(text: &str) -> Result<Self, PatchError> {
        if let Some(found) = invalid_escape(text) {
            return Err(PatchError::Failed(format!("Found invalid escape {found}")));
        }
        let mut segments = text.split('/');
        if segments.next() != Some("") {
            return Err(PatchError::Failed("Location must start with /".to_owned()));
        }
        Ok(Self {
            parts: segments.map(unescape).collect(),
        })
    }

    /// `JsonPointer.contains`.
    fn contains(&self, other: &Self) -> bool {
        self.parts.starts_with(&other.parts)
    }
}

/// `_RE_INVALID_ESCAPE`: a `~` that is last, or not followed by `0` or `1`.
fn invalid_escape(text: &str) -> Option<String> {
    let mut chars = text.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '~' {
            continue;
        }
        return match chars.peek() {
            Some((_, '0' | '1')) => {
                chars.next();
                continue;
            }
            Some((_, next)) => Some(format!("~{next}")),
            None => Some("~".to_owned()),
        };
    }
    None
}

fn unescape(part: &str) -> String {
    part.replace("~1", "/").replace("~0", "~")
}

/// One resolved step of a pointer, in the type the parent needs it in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
    /// `-`, the position past the end of a list.
    Append,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(k) => f.write_str(k),
            Self::Index(i) => write!(f, "{i}"),
            Self::Append => f.write_str("-"),
        }
    }
}

/// `JsonPointer.get_part`.
fn get_part(doc: &Value, part: &str) -> Result<Step, PatchError> {
    match doc {
        // `str` defines `__getitem__`, so jsonpointer ducktypes it as
        // indexable alongside a mapping and the failure surfaces one step
        // later.
        Value::Object(_) | Value::String(_) => Ok(Step::Key(part.to_owned())),
        Value::Array(_) => {
            if part == "-" {
                return Ok(Step::Append);
            }
            parse_index(part).map(Step::Index)
        }
        other => Err(PatchError::Failed(format!(
            "Document '{}' does not support indexing, must be mapping/sequence \
             or support __getitem__",
            py_type(other)
        ))),
    }
}

/// `_RE_ARRAY_INDEX` followed by `int()`.
///
/// The regex is `0|[1-9][0-9]*$`, which anchors the end of the second branch
/// only, so anything starting with `0` is accepted and handed to `int()`. That
/// makes `/list/01` a valid reference to element 1 — RFC 6901 forbids leading
/// zeros — and `/list/0abc` a `ValueError` rather than a pointer error. Both
/// are reproduced here; see docs/COMPAT.md.
fn parse_index(part: &str) -> Result<usize, PatchError> {
    let accepted = part.starts_with('0')
        || (part.starts_with(|c: char| c.is_ascii_digit() && c != '0')
            && part.bytes().all(|b| b.is_ascii_digit()));
    if !accepted {
        return Err(PatchError::Failed(format!(
            "'{part}' is not a valid sequence index"
        )));
    }
    if part.bytes().all(|b| b.is_ascii_digit()) {
        // Python has no integer ceiling; an index this large can only ever be
        // out of bounds, which is the same answer saturating gives.
        Ok(part.parse::<usize>().unwrap_or(usize::MAX))
    } else {
        Err(PatchError::Value(format!(
            "invalid literal for int() with base 10: '{part}'"
        )))
    }
}

/// `JsonPointer.walk`: one step down, or the error that step produced.
fn descend<'a>(doc: &'a mut Value, step: &Step) -> Result<&'a mut Value, PatchError> {
    match (doc, step) {
        (Value::Object(map), Step::Key(key)) => map
            .get_mut(key)
            .ok_or_else(|| PatchError::Failed(format!("member '{key}' not found"))),
        (Value::Array(items), Step::Index(index)) => {
            items.get_mut(*index).ok_or_else(|| {
                PatchError::Failed(format!("index '{index}' is out of bounds"))
            })
        }
        // `EndOfList` is a bare marker object with no `__getitem__`.
        (Value::Array(_), Step::Append) => Err(PatchError::Failed(
            "Document 'EndOfList' does not support indexing, must be \
             mapping/sequence or support __getitem__"
                .to_owned(),
        )),
        (Value::String(_), _) => Err(PatchError::Failed(
            "string indices must be integers".to_owned(),
        )),
        (other, step) => Err(PatchError::Failed(format!(
            "cannot resolve {step} in {}",
            py_type(other)
        ))),
    }
}

/// `JsonPointer.to_last`: the container the last step applies to, and that step.
/// An empty pointer has no last step, which is how the root is addressed.
fn to_last<'a>(
    doc: &'a mut Value,
    parts: &[String],
) -> Result<(&'a mut Value, Option<Step>), PatchError> {
    let Some((last, leading)) = parts.split_last() else {
        return Ok((doc, None));
    };
    let mut current = doc;
    for part in leading {
        let step = get_part(current, part)?;
        current = descend(current, &step)?;
    }
    let step = get_part(current, last)?;
    Ok((current, Some(step)))
}

/// `subobj[part]` in the `move` and `copy` operations, which read before they
/// write. `KeyError`/`IndexError` become conflicts; anything else propagates.
fn fetch(parent: &mut Value, step: Option<&Step>) -> Result<Value, PatchError> {
    match step {
        Some(step) => descend(parent, step).map(|found| found.clone()),
        None => Err(PatchError::Failed("None".to_owned())),
    }
}

/// The operations `JsonPatch.operations` maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Remove,
    Replace,
    Move,
    Copy,
    Test,
}

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "add" => Self::Add,
            "remove" => Self::Remove,
            "replace" => Self::Replace,
            "move" => Self::Move,
            "copy" => Self::Copy,
            "test" => Self::Test,
            _ => return None,
        })
    }
}

/// `PatchOperation`. `value` and `from` are looked up when the operation runs,
/// not when it is parsed, which is why the whole object is kept.
#[derive(Debug, Clone)]
struct Operation {
    kind: Kind,
    location: String,
    pointer: Pointer,
    members: Object,
}

impl Operation {
    fn parse(raw: &Value) -> Result<Self, PatchError> {
        let Some(members) = raw.as_object() else {
            return Err(PatchError::Failed(
                "Operation does not contain 'op' member".to_owned(),
            ));
        };
        let Some(op) = members.get("op") else {
            return Err(PatchError::Failed(
                "Operation does not contain 'op' member".to_owned(),
            ));
        };
        let Some(op) = op.as_str() else {
            return Err(PatchError::Failed("Operation must be a string".to_owned()));
        };
        let kind = Kind::parse(op)
            .ok_or_else(|| PatchError::Failed(format!("Unknown operation '{op}'")))?;
        let Some(path) = members.get("path") else {
            return Err(PatchError::Failed(
                "Operation must have a 'path' member".to_owned(),
            ));
        };
        // A non-string path reaches the escape regex, which raises `TypeError`;
        // the library turns that into `Invalid 'path'`. Duplicate `path` keys
        // land here too, because `loads` folds them into a list.
        let path = path
            .as_str()
            .ok_or_else(|| PatchError::Failed("Invalid 'path'".to_owned()))?;
        Ok(Self {
            kind,
            location: path.to_owned(),
            pointer: Pointer::parse(path)?,
            members: members.clone(),
        })
    }

    fn value(&self) -> Result<Value, PatchError> {
        self.members.get("value").cloned().ok_or_else(|| {
            PatchError::Failed(
                "The operation does not contain a 'value' member".to_owned(),
            )
        })
    }

    fn from(&self) -> Result<Pointer, PatchError> {
        let raw = self.members.get("from").ok_or_else(|| {
            PatchError::Failed(
                "The operation does not contain a 'from' member".to_owned(),
            )
        })?;
        let text = raw
            .as_str()
            .ok_or_else(|| PatchError::Failed("Invalid 'from'".to_owned()))?;
        Pointer::parse(text)
    }

    fn apply(&self, doc: Value) -> Result<Value, PatchError> {
        match self.kind {
            Kind::Add => self.add(doc, self.value()?),
            Kind::Remove => self.remove(doc),
            Kind::Replace => self.replace(doc),
            Kind::Move => self.move_(doc),
            Kind::Copy => self.copy(doc),
            Kind::Test => self.test(doc),
        }
    }

    /// `AddOperation`.
    fn add(&self, doc: Value, value: Value) -> Result<Value, PatchError> {
        self.add_at(doc, &self.pointer.parts.clone(), value)
    }

    fn add_at(
        &self,
        mut doc: Value,
        parts: &[String],
        value: Value,
    ) -> Result<Value, PatchError> {
        {
            let (parent, step) = to_last(&mut doc, parts)?;
            match (parent, step) {
                (_, None) => {
                    // Only a mapping root can be replaced wholesale: upstream
                    // compares `None > len(list)` for a list, which is a
                    // `TypeError`, and rejects every other type outright.
                    return match &doc {
                        Value::Object(_) => Ok(value),
                        Value::Array(_) => Err(PatchError::Failed(
                            "'>' not supported between instances of 'NoneType' and 'int'"
                                .to_owned(),
                        )),
                        other => Err(PatchError::Failed(format!(
                            "invalid document type {}",
                            py_type(other)
                        ))),
                    };
                }
                (Value::Array(items), Some(Step::Append)) => items.push(value),
                (Value::Array(items), Some(Step::Index(index))) => {
                    if index > items.len() {
                        return Err(PatchError::Failed(
                            "can't insert outside of list".to_owned(),
                        ));
                    }
                    items.insert(index, value);
                }
                (Value::Object(map), Some(Step::Key(key))) => {
                    map.insert(key, value);
                }
                (parent, Some(step)) => {
                    return Err(PatchError::Failed(format!(
                        "unable to fully resolve json pointer {}, part {step} in {}",
                        self.location,
                        py_type(parent)
                    )))
                }
            }
        }
        Ok(doc)
    }

    /// `RemoveOperation`.
    fn remove(&self, doc: Value) -> Result<Value, PatchError> {
        Self::remove_at(doc, &self.pointer.parts.clone())
    }

    fn remove_at(mut doc: Value, parts: &[String]) -> Result<Value, PatchError> {
        {
            let (parent, step) = to_last(&mut doc, parts)?;
            match (parent, step) {
                (Value::Object(map), Some(Step::Key(key))) => {
                    if map.shift_remove(&key).is_none() {
                        return Err(PatchError::Failed(format!(
                            "can't remove a non-existent object '{key}'"
                        )));
                    }
                }
                (Value::Array(items), Some(Step::Index(index))) => {
                    if index >= items.len() {
                        return Err(PatchError::Failed(format!(
                            "can't remove a non-existent object '{index}'"
                        )));
                    }
                    items.remove(index);
                }
                // `del d[None]` on a mapping is a `KeyError`; on anything else
                // it is a `TypeError`. Both leave the document untouched.
                (Value::Object(_), None) => {
                    return Err(PatchError::Failed(
                        "can't remove a non-existent object 'None'".to_owned(),
                    ))
                }
                (parent, step) => {
                    return Err(PatchError::Failed(format!(
                        "cannot delete {} from {}",
                        step.map_or_else(|| "None".to_owned(), |s| s.to_string()),
                        py_type(parent)
                    )))
                }
            }
        }
        Ok(doc)
    }

    /// `ReplaceOperation`.
    fn replace(&self, mut doc: Value) -> Result<Value, PatchError> {
        let value = self.value()?;
        {
            let (parent, step) = to_last(&mut doc, &self.pointer.parts)?;
            let Some(step) = step else {
                return Ok(value);
            };
            match (parent, step) {
                (_, Step::Append) => {
                    return Err(PatchError::Failed(
                        "'path' with '-' can't be applied to 'replace' operation"
                            .to_owned(),
                    ))
                }
                (Value::Array(items), Step::Index(index)) => {
                    let Some(slot) = items.get_mut(index) else {
                        return Err(PatchError::Failed(
                            "can't replace outside of list".to_owned(),
                        ));
                    };
                    *slot = value;
                }
                (Value::Object(map), Step::Key(key)) => {
                    if !map.contains_key(&key) {
                        return Err(PatchError::Failed(format!(
                            "can't replace a non-existent object '{key}'"
                        )));
                    }
                    map.insert(key, value);
                }
                (parent, step) => {
                    return Err(PatchError::Failed(format!(
                        "unable to fully resolve json pointer {}, part {step} in {}",
                        self.location,
                        py_type(parent)
                    )))
                }
            }
        }
        Ok(doc)
    }

    /// `MoveOperation`: a read, then a remove, then an add.
    fn move_(&self, mut doc: Value) -> Result<Value, PatchError> {
        let from = self.from()?;
        let (value, from_is_mapping) = {
            let (parent, step) = to_last(&mut doc, &from.parts)?;
            let is_mapping = parent.is_object();
            (fetch(parent, step.as_ref())?, is_mapping)
        };
        if self.pointer == from {
            return Ok(doc);
        }
        if from_is_mapping && self.pointer.contains(&from) {
            return Err(PatchError::Failed(
                "Cannot move values into their own children".to_owned(),
            ));
        }
        let doc = Self::remove_at(doc, &from.parts)?;
        self.add_at(doc, &self.pointer.parts.clone(), value)
    }

    /// `CopyOperation`.
    fn copy(&self, mut doc: Value) -> Result<Value, PatchError> {
        let from = self.from()?;
        let value = {
            let (parent, step) = to_last(&mut doc, &from.parts)?;
            fetch(parent, step.as_ref())?
        };
        self.add_at(doc, &self.pointer.parts.clone(), value)
    }

    /// `TestOperation`.
    fn test(&self, mut doc: Value) -> Result<Value, PatchError> {
        let expected = self.value()?;
        {
            let (parent, step) = to_last(&mut doc, &self.pointer.parts)?;
            let found = match step {
                // `EndOfList` is never equal to a JSON value, so `-` always
                // fails the test rather than reading the last element.
                Some(Step::Append) => {
                    return Err(PatchError::Failed(
                        "EndOfList is not equal to tested value".to_owned(),
                    ))
                }
                Some(step) => descend(parent, &step)?,
                None => parent,
            };
            if !python_eq(found, &expected) {
                return Err(PatchError::Failed(format!(
                    "{found} ({}) is not equal to tested value {expected} ({})",
                    py_type(found),
                    py_type(&expected)
                )));
            }
        }
        Ok(doc)
    }
}

/// Python's `==` over decoded JSON, which is not `serde_json`'s.
///
/// `True == 1` and `1 == 1.0` are both true there, and a `test` operation can
/// see either, because the patch and the document are decoded separately.
fn python_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| python_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|other| python_eq(v, other)))
        }
        _ => match (as_number(left), as_number(right)) {
            (Some(a), Some(b)) => numbers_eq(a, b),
            _ => false,
        },
    }
}

/// `bool` is a subclass of `int` in Python, so it compares as one.
#[derive(Debug, Clone, Copy)]
enum Number {
    Int(i128),
    Float(f64),
}

fn as_number(value: &Value) -> Option<Number> {
    match value {
        Value::Bool(b) => Some(Number::Int(i128::from(*b))),
        Value::Number(n) => n
            .as_i64()
            .map(|i| Number::Int(i128::from(i)))
            .or_else(|| n.as_u64().map(|u| Number::Int(i128::from(u))))
            .or_else(|| n.as_f64().map(Number::Float)),
        _ => None,
    }
}

#[allow(clippy::cast_precision_loss)]
fn numbers_eq(left: Number, right: Number) -> bool {
    match (left, right) {
        (Number::Int(a), Number::Int(b)) => a == b,
        (Number::Float(a), Number::Float(b)) => a == b,
        (Number::Int(a), Number::Float(b)) | (Number::Float(b), Number::Int(a)) => {
            a as f64 == b
        }
    }
}

/// A parsed `#cloud-config-jsonp` body.
#[derive(Debug, Clone)]
pub struct Patch {
    operations: Vec<Operation>,
}

impl Patch {
    /// `JsonPatch.from_string`. Every operation is structurally validated here,
    /// so one malformed operation stops the whole patch before any of it runs.
    pub fn parse(text: &str) -> Result<Self, PatchError> {
        let document = loads(text)?;
        let Value::Array(raw) = document else {
            return Err(PatchError::Failed(format!(
                "'{}' object is not iterable as a patch",
                py_type(&document)
            )));
        };
        Ok(Self {
            operations: raw
                .iter()
                .map(Operation::parse)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    /// `JsonPatch.apply(obj, in_place=False)`: the document is left untouched
    /// when any operation fails.
    pub fn apply(&self, doc: &Value) -> Result<Value, PatchError> {
        let mut out = doc.clone();
        for operation in &self.operations {
            out = operation.apply(out)?;
        }
        Ok(out)
    }
}

/// `jsonpatch._jsonloads`, whose `object_pairs_hook` turns duplicate keys into
/// a list of their values instead of letting the last one win.
fn loads(text: &str) -> Result<Value, PatchError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = AnyValue
        .deserialize(&mut deserializer)
        .map_err(|e| PatchError::Value(e.to_string()))?;
    deserializer
        .end()
        .map_err(|e| PatchError::Value(e.to_string()))?;
    Ok(value)
}

/// Builds a [`Value`] the way `serde_json` does, except for duplicate keys.
#[derive(Debug, Clone, Copy)]
struct AnyValue;

impl<'de> serde::de::DeserializeSeed<'de> for AnyValue {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for AnyValue {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(AnyValue)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Object::new();
        let mut folded: HashSet<String> = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(AnyValue)?;
            match out.get_mut(&key) {
                None => {
                    out.insert(key, value);
                }
                Some(Value::Array(items)) if folded.contains(&key) => items.push(value),
                Some(slot) => {
                    let first = slot.take();
                    *slot = Value::Array(vec![first, value]);
                    folded.insert(key);
                }
            }
        }
        Ok(Value::Object(out))
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

    fn apply(doc: &str, patch: &str) -> Result<Value, PatchError> {
        let doc: Value = serde_json::from_str(doc).unwrap();
        Patch::parse(patch)?.apply(&doc)
    }

    fn applied(doc: &str, patch: &str) -> Value {
        apply(doc, patch).unwrap()
    }

    fn failed(doc: &str, patch: &str) -> PatchError {
        apply(doc, patch).unwrap_err()
    }

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn the_six_operations_do_what_rfc_6902_says() {
        assert_eq!(
            applied("{}", r#"[{"op":"add","path":"/a","value":1}]"#),
            json(r#"{"a":1}"#)
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"remove","path":"/a"}]"#),
            json("{}")
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"replace","path":"/a","value":2}]"#),
            json(r#"{"a":2}"#)
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"move","from":"/a","path":"/b"}]"#),
            json(r#"{"b":1}"#)
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"copy","from":"/a","path":"/b"}]"#),
            json(r#"{"a":1,"b":1}"#)
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"test","path":"/a","value":1}]"#),
            json(r#"{"a":1}"#)
        );
    }

    #[test]
    fn a_list_is_inserted_into_and_appended_to() {
        assert_eq!(
            applied(
                r#"{"a":[1,2,3]}"#,
                r#"[{"op":"add","path":"/a/-","value":4}]"#
            ),
            json(r#"{"a":[1,2,3,4]}"#)
        );
        assert_eq!(
            applied(
                r#"{"a":[1,2,3]}"#,
                r#"[{"op":"add","path":"/a/0","value":0}]"#
            ),
            json(r#"{"a":[0,1,2,3]}"#)
        );
        assert_eq!(
            applied(
                r#"{"a":[1,2,3]}"#,
                r#"[{"op":"add","path":"/a/3","value":4}]"#
            ),
            json(r#"{"a":[1,2,3,4]}"#)
        );
        assert_eq!(
            failed(r#"{"a":[1]}"#, r#"[{"op":"add","path":"/a/5","value":1}]"#),
            PatchError::Failed("can't insert outside of list".to_owned())
        );
    }

    #[test]
    fn an_empty_pointer_addresses_the_root() {
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"add","path":"","value":{"z":9}}]"#),
            json(r#"{"z":9}"#)
        );
        assert_eq!(
            applied(r#"{"a":1}"#, r#"[{"op":"replace","path":"","value":[1]}]"#),
            json("[1]")
        );
        // `del doc[None]` is a KeyError, so the root cannot be removed.
        assert!(
            !failed(r#"{"a":1}"#, r#"[{"op":"remove","path":""}]"#).is_value_error()
        );
    }

    #[test]
    fn a_leading_zero_index_is_accepted_and_a_non_numeric_one_is_a_value_error() {
        // RFC 6901 forbids both; jsonpointer's regex lets both through.
        assert_eq!(
            applied(
                r#"{"a":[1,2,3]}"#,
                r#"[{"op":"add","path":"/a/01","value":9}]"#
            ),
            json(r#"{"a":[1,9,2,3]}"#)
        );
        let err = failed(
            r#"{"a":[1,2,3]}"#,
            r#"[{"op":"add","path":"/a/0abc","value":9}]"#,
        );
        assert!(err.is_value_error(), "{err}");
        assert!(
            !failed(r#"{"a":[1]}"#, r#"[{"op":"add","path":"/a/-1","value":9}]"#)
                .is_value_error()
        );
    }

    #[test]
    fn only_malformed_json_and_a_bad_index_are_value_errors() {
        for patch in [
            r#"[{"op":"bogus","path":"/a"}]"#,
            r#"[{"op":"add","value":1}]"#,
            r#"[{"op":"add","path":"/a"}]"#,
            r#"[{"op":"add","path":"a","value":1}]"#,
            r#"[{"op":"add","path":"/a~2b","value":1}]"#,
            r#"[{"op":"remove","path":"/nope"}]"#,
            r#"[{"op":"test","path":"/a","value":2}]"#,
            r#"["notadict"]"#,
            r#"[{"op":123,"path":"/a"}]"#,
            "null",
        ] {
            let err = failed(r#"{"a":1}"#, patch);
            assert!(!err.is_value_error(), "{patch}: {err}");
        }
        assert!(failed(r#"{"a":1}"#, "not json").is_value_error());
    }

    #[test]
    fn a_failing_operation_leaves_the_document_untouched() {
        let doc = json(r#"{"a":1}"#);
        let patch = Patch::parse(
            r#"[{"op":"add","path":"/b","value":2},{"op":"remove","path":"/nope"}]"#,
        )
        .unwrap();
        assert!(patch.apply(&doc).is_err());
        assert_eq!(doc, json(r#"{"a":1}"#));
    }

    #[test]
    fn duplicate_keys_become_a_list() {
        assert_eq!(
            applied("{}", r#"[{"op":"add","path":"/a","value":1,"value":2}]"#),
            json(r#"{"a":[1,2]}"#)
        );
        assert_eq!(
            applied(
                "{}",
                r#"[{"op":"add","path":"/a","value":{"x":1,"x":2,"x":3}}]"#
            ),
            json(r#"{"a":{"x":[1,2,3]}}"#)
        );
        // A duplicated `path` folds into a list, which is not a valid pointer.
        assert_eq!(
            failed("{}", r#"[{"op":"add","path":"/a","path":"/b","value":1}]"#),
            PatchError::Failed("Invalid 'path'".to_owned())
        );
    }

    #[test]
    fn test_compares_the_way_python_does() {
        assert!(
            apply(r#"{"a":1}"#, r#"[{"op":"test","path":"/a","value":true}]"#).is_ok()
        );
        assert!(
            apply(r#"{"a":1}"#, r#"[{"op":"test","path":"/a","value":1.0}]"#).is_ok()
        );
        assert!(
            apply(r#"{"a":0}"#, r#"[{"op":"test","path":"/a","value":false}]"#).is_ok()
        );
        assert!(
            apply(r#"{"a":"1"}"#, r#"[{"op":"test","path":"/a","value":1}]"#).is_err()
        );
        assert!(apply(
            r#"{"a":{"b":[1,true]}}"#,
            r#"[{"op":"test","path":"/a","value":{"b":[1.0,1]}}]"#
        )
        .is_ok());
        // `-` resolves to the end-of-list marker, which equals nothing.
        assert!(
            apply(r#"{"a":[1]}"#, r#"[{"op":"test","path":"/a/-","value":1}]"#)
                .is_err()
        );
    }

    #[test]
    fn move_is_a_no_op_onto_itself_and_refuses_its_own_children() {
        assert_eq!(
            applied(
                r#"{"a":{"b":1}}"#,
                r#"[{"op":"move","from":"/a/b","path":"/a/b"}]"#
            ),
            json(r#"{"a":{"b":1}}"#)
        );
        assert_eq!(
            failed(
                r#"{"a":{"b":1}}"#,
                r#"[{"op":"move","from":"/a","path":"/a/c"}]"#
            ),
            PatchError::Failed("Cannot move values into their own children".to_owned())
        );
        // The same shape inside a list is allowed, because the guard only
        // checks mappings.
        assert_eq!(
            applied(
                r#"{"a":[1,2]}"#,
                r#"[{"op":"move","from":"/a/0","path":"/a/1"}]"#
            ),
            json(r#"{"a":[2,1]}"#)
        );
    }

    #[test]
    fn a_copy_does_not_alias_its_source() {
        assert_eq!(
            applied(
                r#"{"a":[[1]]}"#,
                r#"[{"op":"copy","from":"/a/0","path":"/b"},
                    {"op":"add","path":"/b/0","value":9}]"#
            ),
            json(r#"{"a":[[1]],"b":[9,1]}"#)
        );
    }

    #[test]
    fn a_pointer_escape_round_trips() {
        assert_eq!(
            applied(
                "{}",
                r#"[{"op":"add","path":"/~0","value":1},{"op":"add","path":"/~1","value":2}]"#
            ),
            json(r#"{"~":1,"/":2}"#)
        );
        assert_eq!(invalid_escape("/a~2b").as_deref(), Some("~2"));
        assert_eq!(invalid_escape("/a~").as_deref(), Some("~"));
        assert_eq!(invalid_escape("/a~0~1b"), None);
    }

    #[test]
    fn a_scalar_cannot_be_walked_through() {
        for doc in [r#"{"a":1}"#, r#"{"a":true}"#, r#"{"a":null}"#] {
            let err = failed(doc, r#"[{"op":"add","path":"/a/b","value":1}]"#);
            assert!(
                err.to_string().contains("does not support indexing"),
                "{err}"
            );
        }
        // A string ducktypes as indexable, so it fails one step later instead.
        let err = failed(
            r#"{"a":"xyz"}"#,
            r#"[{"op":"add","path":"/a/b/c","value":1}]"#,
        );
        assert!(err.to_string().contains("string indices"), "{err}");
    }

    #[test]
    fn an_empty_patch_returns_the_document() {
        assert_eq!(applied(r#"{"a":1}"#, "[]"), json(r#"{"a":1}"#));
    }
}
