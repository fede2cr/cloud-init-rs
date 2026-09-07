//! JSON Schema draft-04 validation, reproducing the Python `jsonschema`
//! library's error messages and cloud-init's overrides on top of it.
//!
//! Upstream builds its validator with
//! `jsonschema.validators.create(version="draft4", ...)` and then replaces
//! `anyOf`, `oneOf`, and adds `deprecated`/`changed`
//! (`cloudinit/config/schema.py::get_jsonschema_validator`). The message
//! strings are part of `cloud-init schema` output, so they are reproduced
//! verbatim rather than paraphrased.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::repr::repr;

/// One step of an instance path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

impl std::fmt::Display for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(k) => f.write_str(k),
            Self::Index(i) => write!(f, "{i}"),
        }
    }
}

/// Whether a finding is a hard error or one of cloud-init's schema annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Error,
    /// `SchemaDeprecationError`, carrying `deprecated_version`.
    Deprecation {
        version: String,
    },
}

/// A single validation finding.
#[derive(Debug, Clone)]
pub struct Error {
    pub path: Vec<Seg>,
    pub message: String,
    pub keyword: String,
    pub kind: Kind,
    /// Sub-errors of a failed `anyOf`/`oneOf`, used by `best_match`.
    pub context: Vec<Error>,
    /// Whether the instance matched the `type` of the schema that rejected it.
    /// Precomputed because `relevance` needs it after the schema is out of scope.
    matches_type: bool,
    /// Set once `descend` has stamped the instance path onto this error; nested
    /// errors are already stamped by the recursive call that produced them.
    path_set: bool,
    /// True when the failing schema is the root document, which
    /// `validate_cloudconfig_schema` uses to recover top-level property names.
    pub at_root_schema: bool,
}

impl Error {
    fn new(keyword: &str, message: String) -> Self {
        Self {
            path: Vec::new(),
            message,
            keyword: keyword.to_owned(),
            kind: Kind::Error,
            context: Vec::new(),
            matches_type: false,
            path_set: false,
            at_root_schema: false,
        }
    }

    fn deprecation(keyword: &str, message: String, version: String) -> Self {
        let mut err = Self::new(keyword, message);
        err.kind = Kind::Deprecation { version };
        err
    }

    fn is_deprecation(&self) -> bool {
        matches!(self.kind, Kind::Deprecation { .. })
    }

    /// `.`-joined instance path, the `path` half of a `SchemaProblem`.
    pub fn path_string(&self) -> String {
        self.path
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    /// `jsonschema.exceptions.relevance`, as a sortable key.
    ///
    /// `-len(path)` first, so shallower errors win; then non-`anyOf`/`oneOf`
    /// keywords; then errors whose instance did *not* match the schema's type.
    fn relevance(&self) -> (isize, bool, bool, bool) {
        let weak = matches!(self.keyword.as_str(), "anyOf" | "oneOf");
        #[allow(clippy::cast_possible_wrap)]
        let depth = -(self.path.len() as isize);
        (depth, !weak, false, !self.matches_type)
    }
}

/// A resolved schema document.
#[derive(Debug)]
pub struct Schema {
    root: Value,
}

impl Schema {
    pub fn new(root: Value) -> Self {
        Self { root }
    }

    pub fn as_value(&self) -> &Value {
        &self.root
    }

    /// Validate `instance` against the whole document.
    pub fn validate(&self, instance: &Value) -> Vec<Error> {
        let mut errors = Vec::new();
        self.descend(instance, &self.root, &mut Vec::new(), &mut errors, true);
        errors
    }

    /// Resolve a local `$ref` such as `#/$defs/cc_runcmd`.
    fn resolve<'a>(&'a self, reference: &str) -> Option<&'a Value> {
        let pointer = reference.strip_prefix('#')?;
        if pointer.is_empty() {
            return Some(&self.root);
        }
        self.root.pointer(&decode_pointer(pointer))
    }

    fn is_valid(&self, instance: &Value, schema: &Value) -> bool {
        let mut errors = Vec::new();
        self.descend(instance, schema, &mut Vec::new(), &mut errors, false);
        // `is_valid` is overridden upstream to ignore deprecation annotations.
        !errors.iter().any(|e| !e.is_deprecation())
    }

    /// `Validator.iter_errors`: run every applicable keyword over `instance`.
    fn descend(
        &self,
        instance: &Value,
        schema: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
        at_root: bool,
    ) {
        // A boolean schema short-circuits everything else.
        if let Value::Bool(ok) = schema {
            if !*ok {
                out.push(Error::new("", format!("{} is not allowed", repr(instance))));
            }
            return;
        }
        let Some(schema) = schema.as_object() else {
            return;
        };

        // draft-04 `ignore_ref_siblings`: `$ref` suppresses every sibling keyword.
        if let Some(Value::String(reference)) = schema.get("$ref") {
            let Some(target) = self.resolve(reference) else {
                return;
            };
            self.descend(instance, target, path, out, false);
            return;
        }

        let start = out.len();
        for (keyword, value) in schema {
            self.apply(keyword, value, instance, schema, path, out);
        }
        for err in out.iter_mut().skip(start) {
            if err.path_set {
                continue;
            }
            err.path.clone_from(path);
            err.path_set = true;
            err.matches_type = matches_type(schema, instance);
            err.at_root_schema = at_root;
        }
    }

    #[allow(clippy::too_many_lines)]
    // One arm per keyword, kept flat so it can be diffed against the
    // `Draft4Validator` VALIDATORS table it mirrors.
    fn apply(
        &self,
        keyword: &str,
        value: &Value,
        instance: &Value,
        schema: &Map<String, Value>,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        match keyword {
            "type" => keyword_type(value, instance, out),
            "enum" => keyword_enum(value, instance, out),
            "required" => keyword_required(value, instance, out),
            "properties" => self.keyword_properties(value, instance, path, out),
            "patternProperties" => {
                self.keyword_pattern_properties(value, instance, path, out);
            }
            "additionalProperties" => {
                self.keyword_additional_properties(value, instance, schema, path, out);
            }
            "items" => self.keyword_items(value, instance, path, out),
            "additionalItems" => {
                self.keyword_additional_items(value, instance, schema, path, out);
            }
            "minItems" => length_bound(instance, value, out, true, "minItems"),
            "maxItems" => length_bound(instance, value, out, false, "maxItems"),
            "minLength" => string_bound(instance, value, out, true),
            "maxLength" => string_bound(instance, value, out, false),
            "minProperties" => property_bound(instance, value, out, true),
            "maxProperties" => property_bound(instance, value, out, false),
            "minimum" => numeric_bound(instance, value, schema, out, true),
            "maximum" => numeric_bound(instance, value, schema, out, false),
            "uniqueItems" => keyword_unique_items(value, instance, out),
            "pattern" => keyword_pattern(value, instance, out),
            "format" => keyword_format(value, instance, out),
            "not" => self.keyword_not(value, instance, out),
            "allOf" => self.keyword_all_of(value, instance, path, out),
            "anyOf" => self.keyword_any_of(value, instance, path, out),
            "oneOf" => self.keyword_one_of(value, instance, path, out),
            "deprecated" | "changed" => annotation(keyword, value, schema, out),
            _ => {}
        }
    }

    fn keyword_properties(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let (Some(properties), Some(object)) =
            (value.as_object(), instance.as_object())
        else {
            return;
        };
        for (name, subschema) in properties {
            let Some(child) = object.get(name) else {
                continue;
            };
            path.push(Seg::Key(name.clone()));
            self.descend(child, subschema, path, out, false);
            path.pop();
        }
    }

    fn keyword_pattern_properties(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let (Some(patterns), Some(object)) = (value.as_object(), instance.as_object())
        else {
            return;
        };
        for (pattern, subschema) in patterns {
            let Some(re) = compile(pattern) else {
                continue;
            };
            for (name, child) in object {
                if re.is_match(name) {
                    path.push(Seg::Key(name.clone()));
                    self.descend(child, subschema, path, out, false);
                    path.pop();
                }
            }
        }
    }

    fn keyword_additional_properties(
        &self,
        value: &Value,
        instance: &Value,
        schema: &Map<String, Value>,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(object) = instance.as_object() else {
            return;
        };
        let extras = additional_properties(object, schema);
        if value.is_object() {
            for name in &extras {
                let Some(child) = object.get(name.as_str()) else {
                    continue;
                };
                path.push(Seg::Key(name.clone()));
                self.descend(child, value, path, out, false);
                path.pop();
            }
            return;
        }
        if value != &Value::Bool(false) && !value.is_null() || extras.is_empty() {
            // `elif not aP and extras`: only a falsy `additionalProperties` errors.
            if !matches!(value, Value::Bool(false)) || extras.is_empty() {
                return;
            }
        }

        let joined = extras
            .iter()
            .map(|e| repr(&Value::String(e.clone())))
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(Value::Object(patterns)) = schema.get("patternProperties") {
            let verb = if extras.len() == 1 { "does" } else { "do" };
            let listed = patterns
                .keys()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|p| repr(&Value::String(p.clone())))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Error::new(
                "additionalProperties",
                format!("{joined} {verb} not match any of the regexes: {listed}"),
            ));
        } else {
            let verb = if extras.len() == 1 { "was" } else { "were" };
            out.push(Error::new(
                "additionalProperties",
                format!("Additional properties are not allowed ({joined} {verb} unexpected)"),
            ));
        }
    }

    /// draft-04 `items`: a single schema applies to every element, an array of
    /// schemas applies positionally.
    fn keyword_items(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(items) = instance.as_array() else {
            return;
        };
        match value {
            Value::Array(schemas) => {
                for (index, (item, subschema)) in items.iter().zip(schemas).enumerate()
                {
                    path.push(Seg::Index(index));
                    self.descend(item, subschema, path, out, false);
                    path.pop();
                }
            }
            subschema => {
                for (index, item) in items.iter().enumerate() {
                    path.push(Seg::Index(index));
                    self.descend(item, subschema, path, out, false);
                    path.pop();
                }
            }
        }
    }

    fn keyword_additional_items(
        &self,
        value: &Value,
        instance: &Value,
        schema: &Map<String, Value>,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(items) = instance.as_array() else {
            return;
        };
        // Only meaningful when `items` is an array of schemas.
        let Some(prefix) = schema.get("items").and_then(Value::as_array) else {
            return;
        };
        let extra = items.iter().enumerate().skip(prefix.len());
        if value.is_object() {
            for (index, item) in extra {
                path.push(Seg::Index(index));
                self.descend(item, value, path, out, false);
                path.pop();
            }
        } else if matches!(value, Value::Bool(false)) && items.len() > prefix.len() {
            let count = items.len() - prefix.len();
            let verb = if count == 1 { "was" } else { "were" };
            let joined = items
                .iter()
                .skip(prefix.len())
                .map(repr)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Error::new(
                "additionalItems",
                format!(
                    "Additional items are not allowed ({joined} {verb} unexpected)"
                ),
            ));
        }
    }

    fn keyword_not(&self, value: &Value, instance: &Value, out: &mut Vec<Error>) {
        if self.is_valid(instance, value) {
            out.push(Error::new(
                "not",
                format!(
                    "{} should not be valid under {}",
                    repr(instance),
                    repr(value)
                ),
            ));
        }
    }

    fn keyword_all_of(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(schemas) = value.as_array() else {
            return;
        };
        for subschema in schemas {
            self.descend(instance, subschema, path, out, false);
        }
    }

    /// Upstream's `_anyOf`: deprecation annotations from the matching branch are
    /// kept, and a failure yields `best_match` *plus* the summary error.
    fn keyword_any_of(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(schemas) = value.as_array() else {
            return;
        };
        let mut all_errors: Vec<Error> = Vec::new();
        let mut all_deprecations: Vec<Error> = Vec::new();
        let mut skip_best_match = false;
        let mut matched = false;

        for subschema in schemas {
            let mut branch = Vec::new();
            self.descend(instance, subschema, path, &mut branch, false);
            let (deprecations, errors): (Vec<Error>, Vec<Error>) =
                branch.into_iter().partition(Error::is_deprecation);
            if errors.is_empty() {
                all_deprecations.extend(deprecations);
                matched = true;
                break;
            }
            // The network schema tags its per-`type` branches `anyOf_type_XXX`;
            // when the instance names its own type, that branch's errors are the
            // useful ones and `best_match` is skipped.
            if let (Some(object), Some(reference)) = (
                instance.as_object(),
                subschema.get("$ref").and_then(Value::as_str),
            ) {
                if let Some(Value::String(type_name)) = object.get("type") {
                    if reference.contains("anyOf_type")
                        && reference.contains(&format!("anyOf_type_{type_name}"))
                    {
                        skip_best_match = true;
                        out.extend(errors.iter().cloned());
                    }
                }
            }
            all_errors.extend(errors);
        }

        if matched {
            out.extend(all_deprecations);
            return;
        }
        if !skip_best_match {
            if let Some(best) = best_match(&all_errors) {
                out.push(best);
            }
        }
        let mut summary = Error::new(
            "anyOf",
            format!(
                "{} is not valid under any of the given schemas",
                repr(instance)
            ),
        );
        summary.context = all_errors;
        out.push(summary);
        out.extend(all_deprecations);
    }

    /// Upstream's `_oneOf`, which uses `cloud_init_deepest_matches` instead of
    /// `best_match` and still reports multiple valid branches.
    fn keyword_one_of(
        &self,
        value: &Value,
        instance: &Value,
        path: &mut Vec<Seg>,
        out: &mut Vec<Error>,
    ) {
        let Some(schemas) = value.as_array() else {
            return;
        };
        let mut all_errors: Vec<Error> = Vec::new();
        let mut all_deprecations: Vec<Error> = Vec::new();
        let mut first_valid: Option<&Value> = None;
        let mut rest: &[Value] = &[];

        for (index, subschema) in schemas.iter().enumerate() {
            let mut branch = Vec::new();
            self.descend(instance, subschema, path, &mut branch, false);
            let (deprecations, errors): (Vec<Error>, Vec<Error>) =
                branch.into_iter().partition(Error::is_deprecation);
            if errors.is_empty() {
                first_valid = Some(subschema);
                all_deprecations.extend(deprecations);
                rest = schemas.get(index + 1..).unwrap_or_default();
                break;
            }
            all_errors.extend(errors);
        }

        if first_valid.is_none() {
            out.extend(deepest_matches(&all_errors, instance));
        }

        // `subschemas` is a live enumerate() upstream, so only the branches after
        // the first match are re-tested here.
        let more_valid: Vec<&Value> =
            rest.iter().filter(|s| self.is_valid(instance, s)).collect();
        if more_valid.is_empty() {
            out.extend(all_deprecations);
            return;
        }
        let reprs = more_valid
            .into_iter()
            .chain(first_valid)
            .map(repr)
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Error::new(
            "oneOf",
            format!("{} is valid under each of {reprs}", repr(instance)),
        ));
    }
}

/// `cloudinit.config.schema._validator`: turn a `deprecated`/`changed` marker
/// into an annotation carrying the version it was introduced in.
fn annotation(
    keyword: &str,
    value: &Value,
    schema: &Map<String, Value>,
    out: &mut Vec<Error>,
) {
    if !matches!(value, Value::Bool(true)) {
        return;
    }
    let version = schema
        .get("deprecated_version")
        .and_then(Value::as_str)
        .unwrap_or("devel")
        .to_owned();
    out.push(Error::deprecation(
        keyword,
        deprecation_message(keyword, schema),
        version,
    ));
}

/// `_add_deprecated_changed_or_new_msg(config, annotate=True, filter_key=[key])`.
///
/// `annotate=True` is what the CLI always passes through `_validator`, so the
/// message is a leading-space-prefixed sentence rather than italic RST.
fn deprecation_message(keyword: &str, schema: &Map<String, Value>) -> String {
    let description = schema
        .get(&format!("{keyword}_description"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = schema
        .get(&format!("{keyword}_version"))
        .and_then(Value::as_str)
        .map_or_else(
            || format!("<missing {keyword}_version key, please file a bug report>"),
            ToOwned::to_owned,
        );
    let mut chars = keyword.chars();
    let capitalized = chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    });
    let body = schema
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!("{body} {capitalized} in version {version}. {description}")
        .trim_end()
        .to_owned()
}

/// `jsonschema.exceptions.best_match`.
fn best_match(errors: &[Error]) -> Option<Error> {
    let mut best = errors.first()?.clone();
    for err in errors.iter().skip(1) {
        if err.relevance() > best.relevance() {
            best = err.clone();
        }
    }
    // Descend into context while the two least-relevant sub-errors differ.
    while !best.context.is_empty() {
        let mut sorted: Vec<&Error> = best.context.iter().collect();
        sorted.sort_by_key(|e| e.relevance());
        let (Some(first), Some(second)) = (sorted.first(), sorted.get(1)) else {
            let next = sorted.first().map(|e| (*e).clone());
            match next {
                Some(err) => best = err,
                None => break,
            }
            continue;
        };
        if first.relevance() == second.relevance() {
            break;
        }
        best = (*first).clone();
    }
    Some(best)
}

/// `cloud_init_deepest_matches`: prefer the errors furthest into the instance,
/// or, for a `type`-tagged object, the ones about `type` itself.
fn deepest_matches(errors: &[Error], instance: &Value) -> Vec<Error> {
    let declared_type = instance
        .as_object()
        .and_then(|o| o.get("type"))
        .map(|t| t.as_str().unwrap_or_default().to_owned());

    let mut best: Vec<Error> = Vec::new();
    let mut depth = 0usize;
    for err in errors {
        if declared_type.is_some() {
            if err.path.last() == Some(&Seg::Key("type".to_owned())) {
                best.push(err.clone());
            }
        } else if err.path.len() == depth {
            best.push(err.clone());
        } else if err.path.len() > depth {
            depth = err.path.len();
            best = vec![err.clone()];
        }
    }
    best
}

fn keyword_type(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    let types = as_type_list(value);
    if types.iter().any(|t| is_type(instance, t)) {
        return;
    }
    let reprs = types
        .iter()
        .map(|t| repr(&Value::String((*t).to_owned())))
        .collect::<Vec<_>>()
        .join(", ");
    out.push(Error::new(
        "type",
        format!("{} is not of type {reprs}", repr(instance)),
    ));
}

fn as_type_list(value: &Value) -> Vec<&str> {
    match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// draft-04 type semantics: `integer` accepts a float only when it is not one,
/// and booleans are never numbers.
fn is_type(instance: &Value, name: &str) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
        _ => false,
    }
}

fn matches_type(schema: &Map<String, Value>, instance: &Value) -> bool {
    let Some(expected) = schema.get("type") else {
        return false;
    };
    as_type_list(expected).iter().any(|t| is_type(instance, t))
}

/// `jsonschema._utils.find_additional_properties`, sorted the way the message
/// builder sorts it (`sorted(extras, key=str)`).
fn additional_properties(
    object: &Map<String, Value>,
    schema: &Map<String, Value>,
) -> Vec<String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let patterns: Vec<regex::Regex> = schema
        .get("patternProperties")
        .and_then(Value::as_object)
        .map(|p| p.keys().filter_map(|k| compile(k)).collect())
        .unwrap_or_default();

    let mut extras: Vec<String> = object
        .keys()
        .filter(|name| {
            let known = properties.is_some_and(|p| p.contains_key(name.as_str()));
            !known && !patterns.iter().any(|re| re.is_match(name))
        })
        .cloned()
        .collect();
    extras.sort();
    extras
}

fn keyword_enum(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    let Some(options) = value.as_array() else {
        return;
    };
    if options.iter().any(|o| o == instance) {
        return;
    }
    out.push(Error::new(
        "enum",
        format!("{} is not one of {}", repr(instance), repr(value)),
    ));
}

fn keyword_required(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    let (Some(required), Some(object)) = (value.as_array(), instance.as_object())
    else {
        return;
    };
    for name in required.iter().filter_map(Value::as_str) {
        if !object.contains_key(name) {
            out.push(Error::new(
                "required",
                format!(
                    "{} is a required property",
                    repr(&Value::String(name.to_owned()))
                ),
            ));
        }
    }
}

fn keyword_unique_items(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    if !matches!(value, Value::Bool(true)) {
        return;
    }
    let Some(items) = instance.as_array() else {
        return;
    };
    let unique = items
        .iter()
        .enumerate()
        .all(|(i, a)| !items.iter().take(i).any(|b| b == a));
    if !unique {
        out.push(Error::new(
            "uniqueItems",
            format!("{} has non-unique elements", repr(instance)),
        ));
    }
}

fn keyword_pattern(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    let (Some(pattern), Some(text)) = (value.as_str(), instance.as_str()) else {
        return;
    };
    let Some(re) = compile(pattern) else {
        return;
    };
    if !re.is_match(text) {
        out.push(Error::new(
            "pattern",
            format!("{} does not match {}", repr(instance), repr(value)),
        ));
    }
}

/// The draft-04 format checker, limited to the checks that are actually active
/// in the packaged environment. See docs/COMPAT.md.
fn keyword_format(value: &Value, instance: &Value, out: &mut Vec<Error>) {
    let (Some(format), Some(text)) = (value.as_str(), instance.as_str()) else {
        return;
    };
    if format != "date" || is_iso_date(text) {
        return;
    }
    out.push(Error::new(
        "format",
        format!("{} is not a {}", repr(instance), repr(value)),
    ));
}

/// `datetime.date.fromisoformat`, which is what the `date` checker calls.
fn is_iso_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    let digits_at = |range: std::ops::Range<usize>| {
        text.get(range)
            .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
    };
    if !digits_at(0..4) || !digits_at(5..7) || !digits_at(8..10) {
        return false;
    }
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        text.get(0..4).unwrap_or_default().parse::<u32>(),
        text.get(5..7).unwrap_or_default().parse::<u32>(),
        text.get(8..10).unwrap_or_default().parse::<u32>(),
    ) else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn length_bound(
    instance: &Value,
    value: &Value,
    out: &mut Vec<Error>,
    min: bool,
    keyword: &str,
) {
    let (Some(items), Some(bound)) = (instance.as_array(), value.as_u64()) else {
        return;
    };
    let len = items.len() as u64;
    if min && len < bound {
        out.push(Error::new(
            keyword,
            format!("{} is too short", repr(instance)),
        ));
    } else if !min && len > bound {
        out.push(Error::new(
            keyword,
            format!("{} is too long", repr(instance)),
        ));
    }
}

fn string_bound(instance: &Value, value: &Value, out: &mut Vec<Error>, min: bool) {
    let (Some(text), Some(bound)) = (instance.as_str(), value.as_u64()) else {
        return;
    };
    let len = text.chars().count() as u64;
    if min && len < bound {
        out.push(Error::new(
            "minLength",
            format!("{} is too short", repr(instance)),
        ));
    } else if !min && len > bound {
        out.push(Error::new(
            "maxLength",
            format!("{} is too long", repr(instance)),
        ));
    }
}

fn property_bound(instance: &Value, value: &Value, out: &mut Vec<Error>, min: bool) {
    let (Some(object), Some(bound)) = (instance.as_object(), value.as_u64()) else {
        return;
    };
    let len = object.len() as u64;
    if min && len < bound {
        out.push(Error::new(
            "minProperties",
            format!("{} does not have enough properties", repr(instance)),
        ));
    } else if !min && len > bound {
        out.push(Error::new(
            "maxProperties",
            format!("{} has too many properties", repr(instance)),
        ));
    }
}

/// draft-04 `minimum`/`maximum`, where exclusivity is a sibling boolean.
fn numeric_bound(
    instance: &Value,
    value: &Value,
    schema: &Map<String, Value>,
    out: &mut Vec<Error>,
    min: bool,
) {
    if instance.is_boolean() {
        return;
    }
    let (Some(actual), Some(bound)) = (instance.as_f64(), value.as_f64()) else {
        return;
    };
    let exclusive_key = if min {
        "exclusiveMinimum"
    } else {
        "exclusiveMaximum"
    };
    let exclusive = matches!(schema.get(exclusive_key), Some(Value::Bool(true)));
    let failed = if min {
        if exclusive {
            actual <= bound
        } else {
            actual < bound
        }
    } else if exclusive {
        actual >= bound
    } else {
        actual > bound
    };
    if !failed {
        return;
    }
    let (keyword, relation) = if min {
        (
            "minimum",
            if exclusive {
                "less than or equal to the minimum of"
            } else {
                "less than the minimum of"
            },
        )
    } else {
        (
            "maximum",
            if exclusive {
                "greater than or equal to the maximum of"
            } else {
                "greater than the maximum of"
            },
        )
    };
    out.push(Error::new(
        keyword,
        format!("{} is {relation} {}", repr(instance), repr(value)),
    ));
}

/// Compile a schema regex, ignoring patterns Rust's engine cannot represent.
///
/// Python's `re` is used with `re.search`, so the pattern is not anchored here
/// either.
fn compile(pattern: &str) -> Option<regex::Regex> {
    regex::Regex::new(pattern).ok()
}

/// Expand `~1`/`~0` in a JSON pointer fragment.
fn decode_pointer(pointer: &str) -> String {
    pointer.replace("~1", "/").replace("~0", "~")
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

    fn check(schema: serde_json::Value, instance: &serde_json::Value) -> Vec<String> {
        Schema::new(schema)
            .validate(instance)
            .into_iter()
            .map(|e| format!("{}: {}", e.path_string(), e.message))
            .collect()
    }

    #[test]
    fn reports_type_mismatches_like_jsonschema() {
        assert_eq!(
            check(
                json!({"properties": {"runcmd": {"type": "array"}}}),
                &json!({"runcmd": 5})
            ),
            ["runcmd: 5 is not of type 'array'"]
        );
    }

    #[test]
    fn reports_unexpected_top_level_properties() {
        assert_eq!(
            check(
                json!({"properties": {"a": {}}, "additionalProperties": false}),
                &json!({"bogus_key_here": 1})
            ),
            [": Additional properties are not allowed ('bogus_key_here' was unexpected)"]
        );
    }

    #[test]
    fn pluralises_the_unexpected_property_message() {
        assert_eq!(
            check(
                json!({"properties": {}, "additionalProperties": false}),
                &json!({"b": 1, "a": 2})
            ),
            [": Additional properties are not allowed ('a', 'b' were unexpected)"]
        );
    }

    #[test]
    fn reports_missing_required_properties_in_schema_order() {
        assert_eq!(
            check(json!({"required": ["a", "b"]}), &json!({})),
            [
                ": 'a' is a required property",
                ": 'b' is a required property"
            ]
        );
    }

    #[test]
    fn descends_into_arrays_with_indices() {
        assert_eq!(
            check(
                json!({"properties": {"x": {"items": {"type": "string"}}}}),
                &json!({"x": ["ok", 3]})
            ),
            ["x.1: 3 is not of type 'string'"]
        );
    }

    #[test]
    fn resolves_local_refs() {
        let schema = json!({
            "$defs": {"cmd": {"type": "array"}},
            "properties": {"runcmd": {"$ref": "#/$defs/cmd"}}
        });
        assert_eq!(
            check(schema, &json!({"runcmd": 5})),
            ["runcmd: 5 is not of type 'array'"]
        );
    }

    #[test]
    fn ignores_keywords_that_are_siblings_of_a_ref() {
        // draft-04 `ignore_ref_siblings`: the `type` here must not be applied.
        let schema = json!({
            "$defs": {"anything": {}},
            "properties": {"x": {"$ref": "#/$defs/anything", "type": "string"}}
        });
        assert!(check(schema, &json!({"x": 5})).is_empty());
    }

    #[test]
    fn treats_integers_and_booleans_as_distinct_types() {
        assert_eq!(
            check(json!({"type": "integer"}), &json!(true)),
            ["type: True is not of type 'integer'"].map(|s| s.replace("type: ", ": "))
        );
        assert!(check(json!({"type": "integer"}), &json!(5)).is_empty());
        assert_eq!(
            check(json!({"type": "integer"}), &json!(5.5)),
            [": 5.5 is not of type 'integer'"]
        );
    }

    #[test]
    fn checks_the_date_format() {
        let schema = json!({"properties": {"d": {"format": "date"}}});
        assert!(check(schema.clone(), &json!({"d": "2026-09-01"})).is_empty());
        assert_eq!(
            check(schema.clone(), &json!({"d": "nope"})),
            ["d: 'nope' is not a 'date'"]
        );
        assert_eq!(
            check(schema, &json!({"d": "2026-02-30"})),
            ["d: '2026-02-30' is not a 'date'"]
        );
    }

    #[test]
    fn yields_deprecation_annotations_separately() {
        let schema = json!({
            "properties": {
                "old": {
                    "deprecated": true,
                    "deprecated_version": "22.2",
                    "deprecated_description": "Use **new** instead."
                }
            }
        });
        let errors = Schema::new(schema).validate(&json!({"old": true}));
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].kind,
            Kind::Deprecation {
                version: "22.2".to_owned()
            }
        );
        assert_eq!(
            errors[0].message,
            " Deprecated in version 22.2. Use **new** instead."
        );
    }

    #[test]
    fn matches_pattern_properties() {
        let schema = json!({
            "patternProperties": {"^[0-9]+$": {"type": "string"}},
            "additionalProperties": false
        });
        assert!(check(schema.clone(), &json!({"12": "ok"})).is_empty());
        assert_eq!(
            check(schema, &json!({"ab": "x"})),
            [": 'ab' does not match any of the regexes: '^[0-9]+$'"]
        );
    }

    #[test]
    fn prefers_the_shallowest_error_as_best_match() {
        let deep = Error {
            path: vec![Seg::Key("a".to_owned()), Seg::Key("b".to_owned())],
            ..Error::new("type", "deep".to_owned())
        };
        let shallow = Error {
            path: vec![Seg::Key("a".to_owned())],
            ..Error::new("type", "shallow".to_owned())
        };
        let best = best_match(&[deep, shallow]).unwrap();
        assert_eq!(best.message, "shallow");
    }
}
