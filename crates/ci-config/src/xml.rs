//! A namespace-aware reader for the small XML dialect cloud-init actually
//! meets: Azure's `ovf-env.xml`. It is deliberately not a general parser.
//!
//! A document type declaration is refused outright. Upstream parses with
//! `ET.fromstring`, which rejects external entities but still expands internal
//! ones — bounded by expat's amplification guard, so a few hundred bytes can
//! still become megabytes. Refusing `<!DOCTYPE` removes that class entirely,
//! and no `ovf-env.xml` the platform writes contains one.

use std::collections::BTreeMap;

/// Bounds on a document, so a malformed or hostile file cannot exhaust memory
/// before the structure is understood.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 1 << 20,
            max_depth: 64,
            max_nodes: 8192,
        }
    }
}

/// One attribute, in document order. `namespace` is the resolved URI of the
/// attribute's prefix; an unprefixed attribute is in *no* namespace even when
/// a default `xmlns` is in scope, which is what expat reports and what
/// `ElementTree` stores. The `xmlns` declarations themselves are consumed by
/// the parser and never appear here — a serializer regenerates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub namespace: Option<String>,
    pub name: String,
    pub value: String,
}

/// One element. `text` follows `ElementTree`: the characters before the first
/// child, and `None` when there are none at all. `tail` is the other half of
/// that split — the characters between this element's end tag and its next
/// sibling — which is where everything after the first child ends up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub namespace: Option<String>,
    pub name: String,
    pub attributes: Vec<Attribute>,
    pub text: Option<String>,
    pub tail: Option<String>,
    pub children: Vec<Element>,
}

impl Element {
    /// `ElementTree`'s `elem.tag`: `{uri}local` when namespaced, bare `local`
    /// when not. Upstream matches on this string directly, so the port has to
    /// be able to build it.
    #[must_use]
    pub fn tag(&self) -> String {
        match &self.namespace {
            Some(uri) => format!("{{{uri}}}{}", self.name),
            None => self.name.clone(),
        }
    }

    /// `elem.iter()`: this element and every descendant, in document order.
    pub fn iter(&self) -> impl Iterator<Item = &Element> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let element = stack.pop()?;
            stack.extend(element.children.iter().rev());
            Some(element)
        })
    }

    /// `findall("./ns:name")`: direct children with this qualified name.
    pub fn children_named<'a>(
        &'a self,
        namespace: &str,
        name: &str,
    ) -> impl Iterator<Item = &'a Element> {
        let namespace = namespace.to_owned();
        let name = name.to_owned();
        self.children.iter().filter(move |child| {
            child.name == name && child.namespace.as_deref() == Some(namespace.as_str())
        })
    }

    /// `find("./a/b/c")` over a document with no namespaces. Like
    /// `ElementPath`, this backtracks: a branch that runs out is abandoned and
    /// the next sibling is tried.
    #[must_use]
    pub fn find_path(&self, path: &[&str]) -> Option<&Element> {
        let Some((head, rest)) = path.split_first() else {
            return Some(self);
        };
        self.children
            .iter()
            .filter(|child| child.namespace.is_none() && child.name == *head)
            .find_map(|child| child.find_path(rest))
    }

    /// `find(".//name")`: the first descendant with this name, in document
    /// order. Like `ElementPath`, self is not a candidate.
    #[must_use]
    pub fn find_descendant(&self, name: &str) -> Option<&Element> {
        self.children.iter().find_map(|child| {
            if child.namespace.is_none() && child.name == name {
                Some(child)
            } else {
                child.find_descendant(name)
            }
        })
    }
}

/// Why a document could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Read a document and return its root element.
///
/// # Errors
/// The document is malformed, carries a document type declaration, or exceeds
/// one of `limits`.
pub fn parse(text: &str, limits: Limits) -> Result<Element, Error> {
    if text.len() > limits.max_bytes {
        return Err(Error(format!(
            "document is larger than {} bytes",
            limits.max_bytes
        )));
    }
    Parser {
        rest: text,
        limits,
        nodes: 0,
    }
    .document()
}

struct Parser<'a> {
    rest: &'a str,
    limits: Limits,
    nodes: usize,
}

/// The prefix-to-URI bindings in force, innermost last.
type Scopes = Vec<BTreeMap<String, String>>;

impl<'a> Parser<'a> {
    fn document(mut self) -> Result<Element, Error> {
        let mut scopes: Scopes = Vec::new();
        loop {
            self.skip_whitespace();
            if self.rest.starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.rest.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest.starts_with("<!DOCTYPE") {
                return Err(Error(
                    "document type declarations are not accepted".to_owned(),
                ));
            } else {
                break;
            }
        }
        let root = self.element(&mut scopes, 0)?;
        loop {
            self.skip_whitespace();
            if self.rest.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest.starts_with("<?") {
                self.skip_until("?>")?;
            } else {
                break;
            }
        }
        if !self.rest.is_empty() {
            return Err(Error("trailing data after the root element".to_owned()));
        }
        Ok(root)
    }

    fn element(&mut self, scopes: &mut Scopes, depth: usize) -> Result<Element, Error> {
        if depth > self.limits.max_depth {
            return Err(Error(format!(
                "nesting is deeper than {} elements",
                self.limits.max_depth
            )));
        }
        self.nodes += 1;
        if self.nodes > self.limits.max_nodes {
            return Err(Error(format!(
                "document has more than {} elements",
                self.limits.max_nodes
            )));
        }

        self.expect("<")?;
        let qname = self.take_name()?;
        let (attributes, empty) = self.attributes()?;

        let mut bindings = BTreeMap::new();
        for (key, value) in &attributes {
            let prefix = if key == "xmlns" {
                ""
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                prefix
            } else {
                continue;
            };
            check_reserved(prefix, value)?;
            bindings.insert(prefix.to_owned(), value.clone());
        }
        scopes.push(bindings);

        let result = self.finish_element(scopes, depth, &qname, &attributes, empty);
        scopes.pop();
        result
    }

    fn finish_element(
        &mut self,
        scopes: &mut Scopes,
        depth: usize,
        qname: &str,
        raw_attributes: &[(String, String)],
        empty: bool,
    ) -> Result<Element, Error> {
        let (namespace, name) = resolve(qname, scopes)?;
        let attributes = resolve_attributes(raw_attributes, scopes)?;
        if empty {
            return Ok(Element {
                namespace,
                name,
                attributes,
                text: None,
                tail: None,
                children: Vec::new(),
            });
        }

        let mut text: Option<String> = None;
        let mut children: Vec<Element> = Vec::new();
        loop {
            if self.rest.starts_with("</") {
                self.expect("</")?;
                let closing = self.take_name()?;
                self.skip_whitespace();
                self.expect(">")?;
                if closing != qname {
                    return Err(Error(format!(
                        "mismatched end tag: expected '{qname}', found '{closing}'"
                    )));
                }
                return Ok(Element {
                    namespace,
                    name,
                    attributes,
                    text,
                    tail: None,
                    children,
                });
            }
            if self.rest.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest.starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.rest.starts_with("<![CDATA[") {
                let raw = self.take_cdata()?;
                push_text(&mut text, &mut children, &raw);
            } else if self.rest.starts_with('<') {
                children.push(self.element(scopes, depth + 1)?);
            } else if self.rest.is_empty() {
                return Err(Error(format!("unclosed element '{qname}'")));
            } else {
                let raw = self.take_text();
                let decoded = unescape(raw)?;
                push_text(&mut text, &mut children, &decoded);
            }
        }
    }

    /// Returns the attributes and whether the tag closed itself.
    fn attributes(&mut self) -> Result<(Vec<(String, String)>, bool), Error> {
        let mut attributes = Vec::new();
        loop {
            self.skip_whitespace();
            if let Some(rest) = self.rest.strip_prefix("/>") {
                self.rest = rest;
                return Ok((attributes, true));
            }
            if let Some(rest) = self.rest.strip_prefix('>') {
                self.rest = rest;
                return Ok((attributes, false));
            }
            let key = self.take_name()?;
            self.skip_whitespace();
            self.expect("=")?;
            self.skip_whitespace();
            let Some(quote @ ('"' | '\'')) = self.rest.chars().next() else {
                return Err(Error(format!("attribute '{key}' has no quoted value")));
            };
            self.rest = self.rest.get(quote.len_utf8()..).unwrap_or_default();
            let Some(end) = self.rest.find(quote) else {
                return Err(Error(format!("attribute '{key}' has no closing quote")));
            };
            let raw = self.rest.get(..end).unwrap_or_default();
            let value = unescape(raw)?;
            self.rest = self.rest.get(end + quote.len_utf8()..).unwrap_or_default();
            attributes.push((key, value));
        }
    }

    fn take_name(&mut self) -> Result<String, Error> {
        let end = self
            .rest
            .find(|c: char| {
                !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
            })
            .unwrap_or(self.rest.len());
        let name = self.rest.get(..end).unwrap_or_default();
        if name.is_empty() {
            return Err(Error("expected a name".to_owned()));
        }
        self.rest = self.rest.get(end..).unwrap_or_default();
        Ok(name.to_owned())
    }

    fn take_text(&mut self) -> &'a str {
        let end = self.rest.find('<').unwrap_or(self.rest.len());
        let text = self.rest.get(..end).unwrap_or_default();
        self.rest = self.rest.get(end..).unwrap_or_default();
        text
    }

    fn take_cdata(&mut self) -> Result<String, Error> {
        let body = self.rest.get("<![CDATA[".len()..).unwrap_or_default();
        let Some(end) = body.find("]]>") else {
            return Err(Error("unterminated CDATA section".to_owned()));
        };
        let raw = body.get(..end).unwrap_or_default().to_owned();
        self.rest = body.get(end + "]]>".len()..).unwrap_or_default();
        Ok(raw)
    }

    fn skip_whitespace(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn skip_until(&mut self, marker: &str) -> Result<(), Error> {
        let Some(end) = self.rest.find(marker) else {
            return Err(Error(format!("expected '{marker}'")));
        };
        self.rest = self.rest.get(end + marker.len()..).unwrap_or_default();
        Ok(())
    }

    fn expect(&mut self, marker: &str) -> Result<(), Error> {
        let Some(rest) = self.rest.strip_prefix(marker) else {
            return Err(Error(format!("expected '{marker}'")));
        };
        self.rest = rest;
        Ok(())
    }
}

/// `ElementTree` splits character data in two: everything before the first
/// child is the parent's `.text`, and everything after a child is that
/// *child's* `.tail`. Nothing is discarded, so a document can be written back
/// out unchanged.
fn push_text(text: &mut Option<String>, children: &mut [Element], raw: &str) {
    let slot = match children.last_mut() {
        Some(child) => &mut child.tail,
        None => text,
    };
    slot.get_or_insert_with(String::new).push_str(raw);
}

/// The namespace the `xml` prefix is bound to without any declaration.
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The namespace the declarations themselves live in. Never bindable.
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

/// The namespace declarations expat refuses, and so must this.
///
/// The two reserved URIs may not be bound to a prefix of the caller's choosing,
/// and the two reserved prefixes may not be pointed anywhere else. `xml` is the
/// one exception in each direction: declaring it to its own URI is a legal
/// no-op. Accepting these would let a document address `xml:`-prefixed content
/// under a name a reader resolving it correctly would never produce, which is
/// exactly the sort of disagreement between two parsers that a redaction pass
/// must not be built on.
fn check_reserved(prefix: &str, uri: &str) -> Result<(), Error> {
    if prefix == "xmlns" {
        return Err(Error(
            "the 'xmlns' prefix may not be declared or undeclared".to_owned(),
        ));
    }
    if prefix == "xml" {
        if uri == XML_NAMESPACE {
            return Ok(());
        }
        return Err(Error(
            "the reserved prefix 'xml' may not be bound to another namespace"
                .to_owned(),
        ));
    }
    if uri == XML_NAMESPACE || uri == XMLNS_NAMESPACE {
        return Err(Error(format!(
            "a prefix may not be bound to the reserved namespace '{uri}'"
        )));
    }
    Ok(())
}

/// Resolve an element's attributes, dropping the `xmlns` declarations the
/// parser has already consumed. An unprefixed attribute takes no namespace,
/// even under a default `xmlns`; that is the XML namespaces rule, and it is
/// what `ElementTree` stores.
fn resolve_attributes(
    raw: &[(String, String)],
    scopes: &Scopes,
) -> Result<Vec<Attribute>, Error> {
    let mut attributes = Vec::new();
    for (key, value) in raw {
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let Some((prefix, local)) = key.split_once(':') else {
            attributes.push(Attribute {
                namespace: None,
                name: key.clone(),
                value: value.clone(),
            });
            continue;
        };
        let (namespace, name) = resolve(&format!("{prefix}:{local}"), scopes)?;
        attributes.push(Attribute {
            namespace,
            name,
            value: value.clone(),
        });
    }
    Ok(attributes)
}

/// Split a qualified name and look its prefix up through the open scopes.
fn resolve(qname: &str, scopes: &Scopes) -> Result<(Option<String>, String), Error> {
    let (prefix, local) = match qname.split_once(':') {
        Some((prefix, local)) => (prefix, local),
        None => ("", qname),
    };
    if prefix == "xmlns" {
        return Err(Error(
            "'xmlns' is not usable as an element prefix".to_owned(),
        ));
    }
    let bound = scopes
        .iter()
        .rev()
        .find_map(|scope| scope.get(prefix))
        .cloned();
    match bound {
        Some(uri) => Ok((Some(uri), local.to_owned())),
        None if prefix.is_empty() => Ok((None, local.to_owned())),
        // `xml` is bound whether or not anyone declared it.
        None if prefix == "xml" => {
            Ok((Some(XML_NAMESPACE.to_owned()), local.to_owned()))
        }
        None => Err(Error(format!("unbound namespace prefix '{prefix}'"))),
    }
}

/// The five predefined entities and numeric character references. Anything else
/// is an error, which is also what `ElementTree` does without a DTD.
fn unescape(raw: &str) -> Result<String, Error> {
    if !raw.contains('&') {
        return Ok(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(rest.get(..start).unwrap_or_default());
        let tail = rest.get(start..).unwrap_or_default();
        let Some(end) = tail.find(';') else {
            return Err(Error("unterminated entity reference".to_owned()));
        };
        let entity = tail.get(1..end).unwrap_or_default();
        out.push(decode_entity(entity)?);
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(out)
}

fn decode_entity(entity: &str) -> Result<char, Error> {
    let point = match entity {
        "amp" => return Ok('&'),
        "lt" => return Ok('<'),
        "gt" => return Ok('>'),
        "quot" => return Ok('"'),
        "apos" => return Ok('\''),
        _ => match entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
        {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => entity.strip_prefix('#').and_then(|dec| dec.parse().ok()),
        },
    };
    point
        .and_then(char::from_u32)
        .ok_or_else(|| Error(format!("undefined entity &{entity};")))
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// The prefixes `ElementTree` hands out without being asked, from its module
/// level `_namespace_map`. A document that used `oe:` and `wa:` comes back out
/// with `ns0:` and `ns1:`, but one that used `xsi:` keeps it, because the
/// schema-instance namespace is on this list. Nothing in the port calls
/// `register_namespace`, so the list is fixed.
const WELL_KNOWN_PREFIXES: &[(&str, &str)] = &[
    (XML_NAMESPACE, "xml"),
    ("http://www.w3.org/1999/xhtml", "html"),
    ("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf"),
    ("http://schemas.xmlsoap.org/wsdl/", "wsdl"),
    ("http://www.w3.org/2001/XMLSchema", "xs"),
    ("http://www.w3.org/2001/XMLSchema-instance", "xsi"),
    ("http://purl.org/dc/elements/1.1/", "dc"),
];

/// Write `root` back out the way `ElementTree.tostring(root)` would.
///
/// The output is US-ASCII bytes with no XML declaration, which is what
/// `tostring` produces when `encoding` is left unset. Three things about it
/// are worth knowing before trusting a round trip:
///
/// * **Prefixes are regenerated, not preserved.** The parser resolved every
///   name to a URI and the writer invents a prefix for each URI it meets, in
///   the order it meets them: `ns0`, `ns1`, and so on, except for the handful
///   of URIs in [`WELL_KNOWN_PREFIXES`]. Every declaration lands on the root
///   element and they are emitted sorted by prefix. A namespace that was
///   declared but never used is dropped.
/// * **Comments and processing instructions are gone.** The parser discards
///   them, as does `ET.fromstring` with the default parser.
/// * **Non-ASCII characters become decimal character references**, because the
///   encode step is `us-ascii` with `xmlcharrefreplace`.
///
/// The `ovf-env.xml` Azure actually writes survives all three unchanged: it
/// already uses `ns0:`/`ns1:` and declares `xsi` — which is on the list — and
/// it carries no comments. A live capture round-trips byte for byte.
#[must_use]
pub fn serialize(root: &Element) -> Vec<u8> {
    let prefixes = namespaces(root);
    let mut out = String::new();
    write_element(&mut out, root, &prefixes, true);
    to_ascii(&out)
}

/// `_namespaces`: walk the tree in document order and give every namespace URI
/// a prefix. Returned in first-seen order; the caller sorts when it declares
/// them.
fn namespaces(root: &Element) -> Vec<(String, String)> {
    let mut prefixes: Vec<(String, String)> = Vec::new();
    let add = |uri: &str, prefixes: &mut Vec<(String, String)>| {
        if prefixes.iter().any(|(known, _)| known == uri) {
            return;
        }
        let prefix = WELL_KNOWN_PREFIXES
            .iter()
            .find_map(|(known, prefix)| (*known == uri).then_some((*prefix).to_owned()))
            // `len()` counts the bindings that will be *declared*, so a
            // well-known prefix still advances the counter but `xml` does not.
            .unwrap_or_else(|| format!("ns{}", prefixes.len()));
        if prefix != "xml" {
            prefixes.push((uri.to_owned(), prefix));
        }
    };
    for element in root.iter() {
        if let Some(uri) = &element.namespace {
            add(uri, &mut prefixes);
        }
        for attribute in &element.attributes {
            if let Some(uri) = &attribute.namespace {
                add(uri, &mut prefixes);
            }
        }
    }
    prefixes
}

/// The prefixed name a URI gets. `xml` is never declared but is still usable.
fn qname(namespace: Option<&str>, name: &str, prefixes: &[(String, String)]) -> String {
    let Some(uri) = namespace else {
        return name.to_owned();
    };
    if uri == XML_NAMESPACE {
        return format!("xml:{name}");
    }
    match prefixes
        .iter()
        .find_map(|(known, prefix)| (known == uri).then_some(prefix.as_str()))
    {
        Some(prefix) => format!("{prefix}:{name}"),
        None => name.to_owned(),
    }
}

/// `_serialize_xml`. Only the root carries the `xmlns` declarations.
fn write_element(
    out: &mut String,
    element: &Element,
    prefixes: &[(String, String)],
    root: bool,
) {
    let tag = qname(element.namespace.as_deref(), &element.name, prefixes);
    out.push('<');
    out.push_str(&tag);

    if root {
        let mut declarations: Vec<&(String, String)> = prefixes.iter().collect();
        declarations.sort_by(|left, right| left.1.cmp(&right.1));
        for (uri, prefix) in declarations {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
            out.push_str(&escape_attrib(uri));
            out.push('"');
        }
    }
    for attribute in &element.attributes {
        out.push(' ');
        out.push_str(&qname(
            attribute.namespace.as_deref(),
            &attribute.name,
            prefixes,
        ));
        out.push_str("=\"");
        out.push_str(&escape_attrib(&attribute.value));
        out.push('"');
    }

    let text = element.text.as_deref().unwrap_or_default();
    if text.is_empty() && element.children.is_empty() {
        // `short_empty_elements` is on by default, and the space is upstream's.
        out.push_str(" />");
    } else {
        out.push('>');
        out.push_str(&escape_cdata(text));
        for child in &element.children {
            write_element(out, child, prefixes, false);
        }
        out.push_str("</");
        out.push_str(&tag);
        out.push('>');
    }

    if let Some(tail) = &element.tail {
        out.push_str(&escape_cdata(tail));
    }
}

/// `_escape_cdata`. Only the three that could end an element.
fn escape_cdata(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// `_escape_attrib`. The three above plus the delimiter and the whitespace an
/// attribute-value normalising reader would otherwise fold into spaces. The
/// tab reference really is zero padded to two digits upstream.
fn escape_attrib(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#09;"),
            other => out.push(other),
        }
    }
    out
}

/// `.encode("us-ascii", "xmlcharrefreplace")`, which runs after all escaping,
/// so the references it introduces are never escaped again.
fn to_ascii(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            out.push(character as u8);
        } else {
            out.extend_from_slice(format!("&#{};", character as u32).as_bytes());
        }
    }
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
    use super::{parse, Limits};

    fn root(text: &str) -> super::Element {
        parse(text, Limits::default()).unwrap()
    }

    fn reason(text: &str) -> String {
        parse(text, Limits::default()).unwrap_err().to_string()
    }

    #[test]
    fn text_is_absent_rather_than_empty_when_there_is_none() {
        assert_eq!(root("<a></a>").text, None);
        assert_eq!(root("<a/>").text, None);
        assert_eq!(root("<a> </a>").text.as_deref(), Some(" "));
        assert_eq!(root("<a>  hi  </a>").text.as_deref(), Some("  hi  "));
    }

    #[test]
    fn only_the_text_before_the_first_child_is_kept() {
        let element = root("<a>x<b/>y</a>");
        assert_eq!(element.text.as_deref(), Some("x"));
        assert_eq!(element.children.len(), 1);
    }

    #[test]
    fn predefined_entities_and_character_references_are_decoded() {
        assert_eq!(
            root("<a>&amp;&#65;&#x42;&lt;&gt;</a>").text.as_deref(),
            Some("&AB<>")
        );
        assert_eq!(
            root("<a><![CDATA[<raw>]]></a>").text.as_deref(),
            Some("<raw>")
        );
    }

    #[test]
    fn an_unknown_entity_is_refused_the_way_elementtree_refuses_it() {
        assert_eq!(reason("<a>&xxe;</a>"), "undefined entity &xxe;");
    }

    #[test]
    fn a_doctype_is_refused_outright() {
        let blob = "<!DOCTYPE a [<!ENTITY x \"y\">]>\n<a>&x;</a>";
        assert_eq!(reason(blob), "document type declarations are not accepted");
    }

    #[test]
    fn a_prefix_resolves_through_the_scope_it_was_declared_in() {
        let element =
            root(r#"<r xmlns="urn:d" xmlns:w="urn:w"><w:k>1</w:k><k>2</k></r>"#);
        assert_eq!(element.namespace.as_deref(), Some("urn:d"));
        let qualified: Vec<_> = element.children_named("urn:w", "k").collect();
        assert_eq!(qualified.len(), 1);
        assert_eq!(qualified[0].text.as_deref(), Some("1"));
        assert_eq!(element.children_named("urn:d", "k").count(), 1);
        assert_eq!(element.children_named("urn:w", "missing").count(), 0);
    }

    #[test]
    fn a_path_lookup_backtracks_out_of_a_branch_that_runs_out() {
        let element = root("<r><a><z/></a><a><b>found</b></a></r>");
        assert_eq!(
            element.find_path(&["a", "b"]).and_then(|e| e.text.clone()),
            Some("found".to_owned())
        );
        assert!(element.find_path(&["a", "missing"]).is_none());
        assert_eq!(
            element.find_path(&[]).map(|e| e.name.clone()),
            Some("r".to_owned())
        );
    }

    #[test]
    fn a_descendant_search_takes_the_first_in_document_order_not_the_shallowest() {
        let element = root("<Data><a><Data>deep</Data></a><Data>shallow</Data></Data>");
        // ElementTree's `.//Data` does not consider self, and walks depth
        // first, so the nested one wins over the later sibling.
        assert_eq!(
            element.find_descendant("Data").and_then(|e| e.text.clone()),
            Some("deep".to_owned())
        );
        assert!(element.find_descendant("missing").is_none());
        // A namespaced element does not answer to a bare name.
        assert!(root(r#"<r xmlns="urn:d"><Data>x</Data></r>"#)
            .find_descendant("Data")
            .is_none());
    }

    #[test]
    fn an_unbound_prefix_is_an_error() {
        assert_eq!(reason("<w:r/>"), "unbound namespace prefix 'w'");
    }

    #[test]
    fn declarations_comments_and_processing_instructions_are_skipped() {
        let blob = "<?xml version=\"1.0\"?>\n<!-- lead -->\n<a><!-- in -->hi</a>\n<!-- tail -->";
        assert_eq!(root(blob).text.as_deref(), Some("hi"));
    }

    #[test]
    fn a_mismatched_or_unclosed_tag_is_reported() {
        assert_eq!(
            reason("<a></b>"),
            "mismatched end tag: expected 'a', found 'b'"
        );
        assert_eq!(reason("<a>"), "unclosed element 'a'");
        assert_eq!(reason("<a/><b/>"), "trailing data after the root element");
    }

    #[test]
    fn attributes_may_be_quoted_either_way_and_carry_entities() {
        let element = root(r"<r xmlns:w='urn:&amp;'><w:k/></r>");
        assert_eq!(element.children[0].namespace.as_deref(), Some("urn:&"));
    }

    #[test]
    fn each_limit_is_enforced() {
        let deep = "<a>".repeat(70) + &"</a>".repeat(70);
        let limits = Limits::default();
        assert!(parse(&deep, limits).is_err());
        assert!(parse(
            "<a/>",
            Limits {
                max_bytes: 2,
                ..limits
            }
        )
        .is_err());
        assert!(parse(
            &format!("<r>{}</r>", "<a/>".repeat(20)),
            Limits {
                max_nodes: 10,
                ..limits
            }
        )
        .is_err());
    }

    #[test]
    fn attributes_survive_in_order_and_the_declarations_do_not() {
        let element = root(r#"<r xmlns:w="urn:w" z="1" w:a="2" xmlns="urn:d" m="3"/>"#);
        let names: Vec<&str> = element
            .attributes
            .iter()
            .map(|attribute| attribute.name.as_str())
            .collect();
        assert_eq!(names, ["z", "a", "m"]);
        // An unprefixed attribute is in no namespace even under a default one.
        assert_eq!(element.attributes[0].namespace, None);
        assert_eq!(element.attributes[1].namespace.as_deref(), Some("urn:w"));
        assert_eq!(element.attributes[2].namespace, None);
    }

    #[test]
    fn text_after_a_child_becomes_that_childs_tail() {
        let element = root("<a>lead<b>1</b>tailB<c/>tailC</a>");
        assert_eq!(element.text.as_deref(), Some("lead"));
        assert_eq!(element.children[0].tail.as_deref(), Some("tailB"));
        assert_eq!(element.children[1].tail.as_deref(), Some("tailC"));
        assert_eq!(element.tail, None);
    }

    #[test]
    fn the_xml_prefix_is_bound_without_being_declared() {
        let element = root(r#"<r xml:lang="en"><xml:c/></r>"#);
        assert_eq!(
            element.attributes[0].namespace.as_deref(),
            Some(super::XML_NAMESPACE)
        );
        assert_eq!(
            element.children[0].namespace.as_deref(),
            Some(super::XML_NAMESPACE)
        );
    }

    #[test]
    fn the_reserved_namespaces_may_not_be_rebound() {
        // Declaring `xml` to its own URI is the one legal no-op.
        assert!(parse(
            r#"<r xmlns:xml="http://www.w3.org/XML/1998/namespace"/>"#,
            Limits::default()
        )
        .is_ok());
        for hostile in [
            r#"<r xmlns:p="http://www.w3.org/XML/1998/namespace"/>"#,
            r#"<r xmlns="http://www.w3.org/XML/1998/namespace"/>"#,
            r#"<r xmlns:p="http://www.w3.org/2000/xmlns/"/>"#,
            r#"<r xmlns:xml="urn:elsewhere"/>"#,
            r#"<r xmlns:xmlns="urn:anything"/>"#,
        ] {
            assert!(parse(hostile, Limits::default()).is_err(), "{hostile}");
        }
    }

    fn round_trip(text: &str) -> String {
        String::from_utf8(super::serialize(&root(text))).unwrap()
    }

    #[test]
    fn prefixes_are_regenerated_in_the_order_they_are_first_used() {
        // `urn:B` is declared second but used last, so it is numbered last;
        // the declarations then come out sorted by prefix.
        assert_eq!(
            round_trip(
                r#"<a:R xmlns:a="urn:A" xmlns:b="urn:B" xmlns:c="urn:C"><c:X>1</c:X><b:Y>2</b:Y></a:R>"#
            ),
            r#"<ns0:R xmlns:ns0="urn:A" xmlns:ns1="urn:C" xmlns:ns2="urn:B"><ns1:X>1</ns1:X><ns2:Y>2</ns2:Y></ns0:R>"#
        );
    }

    #[test]
    fn a_declaration_nobody_used_is_dropped_and_a_default_one_gains_a_prefix() {
        assert_eq!(
            round_trip(r#"<R xmlns:unused="urn:U"><C/></R>"#),
            "<R><C /></R>"
        );
        assert_eq!(
            round_trip(r#"<R xmlns="urn:D"><C>1</C></R>"#),
            r#"<ns0:R xmlns:ns0="urn:D"><ns0:C>1</ns0:C></ns0:R>"#
        );
    }

    #[test]
    fn the_well_known_prefixes_keep_their_names_and_xml_is_never_declared() {
        assert_eq!(
            round_trip(
                r#"<R xmlns:w="urn:W" xmlns:s="http://www.w3.org/2001/XMLSchema-instance"><w:C s:nil="true"/></R>"#
            ),
            r#"<R xmlns:ns0="urn:W" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><ns0:C xsi:nil="true" /></R>"#
        );
        assert_eq!(
            round_trip(r#"<R xml:lang="en"/>"#),
            r#"<R xml:lang="en" />"#
        );
    }

    #[test]
    fn an_empty_element_closes_itself_with_a_space_before_the_slash() {
        assert_eq!(
            round_trip("<R><A/><B></B><C> </C></R>"),
            "<R><A /><B /><C> </C></R>"
        );
    }

    #[test]
    fn text_and_attributes_are_escaped_by_different_rules() {
        // Text escapes three characters; an attribute escapes those plus the
        // delimiter and the three whitespace characters, the tab zero padded.
        assert_eq!(
            round_trip("<R k='a&lt;b&gt;c&amp;d&quot;e&apos;f&#10;g&#9;h&#13;i'><T>x&lt;y&gt;z&amp;w&quot;v&apos;u</T></R>"),
            "<R k=\"a&lt;b&gt;c&amp;d&quot;e'f&#10;g&#09;h&#13;i\"><T>x&lt;y&gt;z&amp;w\"v'u</T></R>"
        );
    }

    #[test]
    fn non_ascii_becomes_a_decimal_character_reference() {
        assert_eq!(
            round_trip("<R k='caf\u{e9}'>\u{4e2d}\u{1f600}</R>"),
            r#"<R k="caf&#233;">&#20013;&#128512;</R>"#
        );
    }

    #[test]
    fn comments_and_processing_instructions_do_not_survive_the_round_trip() {
        assert_eq!(
            round_trip("<!-- lead --><R><?pi body?><A/><!-- in --></R>"),
            "<R><A /></R>"
        );
    }

    #[test]
    fn tails_are_written_back_where_they_were_read_from() {
        assert_eq!(
            round_trip("<R>\n <A>1</A>\n <B/>\n</R>"),
            "<R>\n <A>1</A>\n <B />\n</R>"
        );
    }
}
