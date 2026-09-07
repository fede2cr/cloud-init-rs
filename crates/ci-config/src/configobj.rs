//! Port of `configobj` 5.0.9, the parts `cc_mcollective` reaches.
//!
//! `cc_mcollective` parses `/etc/mcollective/server.cfg` with `ConfigObj`,
//! overwrites options from cloud-config and writes the file back, so every
//! byte of that file goes through this code. `configobj` is a hand-written
//! parser built out of four large regular expressions, and its output is
//! shaped by details that look accidental but are load-bearing once a real
//! `server.cfg` is on disk: a value is a list as soon as it contains a comma,
//! quotes around a value are stripped on read and only put back if the value
//! needs them, the space before an inline comment is lost on every rewrite,
//! scalars are always written before sections regardless of the order they
//! were read in, and the indentation of the first indented line in the file
//! decides the indentation of the whole rewritten file.
//!
//! The regexes are lazy, alternated and backtracking, and Python's engine
//! resolves them in a specific order that decides, for instance, whether
//! `a = ,` is an empty list or a parse error. Rather than approximate them,
//! the matchers below are written as continuation-passing backtrackers that
//! offer candidate matches in exactly the order `re` would try them.
//!
//! Not ported: `interpolation` (`write` turns it off and the module never
//! reads a value back), `configspec`/validation, `unrepr`, `list_values=False`
//! and the UTF-16 BOM paths. See `docs/COMPAT.md`.

use serde_json::Value;

/// `DEFAULT_INDENT_TYPE`, used when nothing in the file was indented.
const DEFAULT_INDENT_TYPE: &str = "";

/// `wspace_plus`, the set that forces a value to be quoted when it starts or
/// ends with one of them. Note there is no form feed in it.
fn is_wspace_plus(c: char) -> bool {
    matches!(c, ' ' | '\r' | '\n' | '\u{b}' | '\t' | '\'' | '"')
}

/// Python's `str()` of a config scalar: a string is itself, anything else is
/// its repr.
fn py_str(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| crate::repr::repr(value), ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What `ConfigObj` raises, with `Display` giving Python's message and
/// [`Error::kind`] giving the exception class name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `NestingError`, with the text and the 1-based line.
    Nesting(&'static str, usize),
    /// `DuplicateError`.
    Duplicate(&'static str, usize),
    /// `ParseError`, whose text is built at the raise site because one of the
    /// three forms embeds the offending line.
    Parse(String, usize),
    /// The `ConfigObjError` raised at the end of `_load` when more than one
    /// error was collected.
    Several(usize),
    /// `_quote` refusing a value it cannot round-trip.
    CannotQuote(String),
    /// The `UnicodeDecodeError` from decoding the file as UTF-8.
    Decode {
        /// The offending byte, when the span is a single byte.
        byte: Option<u8>,
        /// 0-based start of the span.
        start: usize,
        /// 0-based end of the span, inclusive.
        end: usize,
        /// Python's reason text.
        reason: &'static str,
    },
    /// The `UnicodeEncodeError` from encoding the result as ASCII.
    Encode {
        /// The offending character, when the span is a single character.
        ch: Option<char>,
        /// 0-based start of the span, in characters.
        start: usize,
        /// 0-based end of the span, inclusive.
        end: usize,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nesting(text, line) | Self::Duplicate(text, line) => {
                write!(f, "{text} at line {line}.")
            }
            Self::Parse(text, line) => write!(f, "{text} at line {line}."),
            Self::Several(line) => write!(
                f,
                "Parsing failed with several errors.\nFirst error at line {line}."
            ),
            Self::CannotQuote(value) => {
                write!(f, "Value \"{value}\" cannot be safely quoted.")
            }
            Self::Decode {
                byte,
                start,
                end,
                reason,
            } => {
                if let Some(byte) = byte {
                    write!(
                        f,
                        "'utf-8' codec can't decode byte 0x{byte:02x} in position \
                         {start}: {reason}"
                    )
                } else {
                    write!(
                        f,
                        "'utf-8' codec can't decode bytes in position {start}-{end}: \
                         {reason}"
                    )
                }
            }
            Self::Encode { ch, start, end } => {
                if let Some(ch) = ch {
                    write!(
                        f,
                        "'ascii' codec can't encode character '{}' in position \
                         {start}: ordinal not in range(128)",
                        escape_char(*ch)
                    )
                } else {
                    write!(
                        f,
                        "'ascii' codec can't encode characters in position \
                         {start}-{end}: ordinal not in range(128)"
                    )
                }
            }
        }
    }
}

impl Error {
    /// The exception class name a caller logging `type(e).__name__` sees.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Nesting(..) => "NestingError",
            Self::Duplicate(..) => "DuplicateError",
            Self::Parse(..) => "ParseError",
            Self::Several(_) | Self::CannotQuote(_) => "ConfigObjError",
            Self::Decode { .. } => "UnicodeDecodeError",
            Self::Encode { .. } => "UnicodeEncodeError",
        }
    }
}

/// How Python spells a character inside an encode error message.
fn escape_char(c: char) -> String {
    let point = c as u32;
    if point < 0x100 {
        format!("\\x{point:02x}")
    } else if point < 0x1_0000 {
        format!("\\u{point:04x}")
    } else {
        format!("\\U{point:08x}")
    }
}

// ---------------------------------------------------------------------------
// The matchers
// ---------------------------------------------------------------------------
//
// Each takes the line as a `&[char]`, a start offset and a continuation. It
// calls the continuation with every end offset the corresponding regex piece
// could produce, in the order Python's engine would try them, and stops at the
// first continuation that returns `true`.

/// The length of the run of whitespace starting at `pos`.
fn ws_run(cs: &[char], pos: usize) -> usize {
    let mut n = 0;
    while cs.get(pos + n).is_some_and(|c| c.is_whitespace()) {
        n += 1;
    }
    n
}

/// `cs[start..end]` as a `String`, and empty when the range is out of bounds.
fn span(cs: &[char], start: usize, end: usize) -> String {
    cs.get(start..end)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

/// `\s*(\#.*)?$`, shared by the value, section-marker and triple-quote
/// regexes. The continuation is handed the comment, which is empty when the
/// optional group did not take part.
fn m_tail(cs: &[char], pos: usize, k: &mut dyn FnMut(&str) -> bool) -> bool {
    let mut p = pos + ws_run(cs, pos);
    loop {
        if cs.get(p) == Some(&'#') && k(&span(cs, p, cs.len())) {
            return true;
        }
        if p == cs.len() && k("") {
            return true;
        }
        if p == pos {
            return false;
        }
        p -= 1;
    }
}

/// `\s*(\#.*)?$` as a plain answer, for the callers that have nothing to
/// backtrack into.
fn tail_at(cs: &[char], pos: usize) -> Option<String> {
    let mut found = None;
    m_tail(cs, pos, &mut |comment| {
        found = Some(comment.to_owned());
        true
    });
    found
}

/// `".*?"` or `'.*?'` -- lazy, so the nearest closing quote is offered first.
fn m_quoted(
    cs: &[char],
    pos: usize,
    q: char,
    k: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if cs.get(pos) != Some(&q) {
        return false;
    }
    let mut e = pos + 1;
    while e < cs.len() {
        if cs.get(e) == Some(&q) && k(e + 1) {
            return true;
        }
        e += 1;
    }
    false
}

/// `[first][rest]*?` -- one required character then a lazy run.
fn m_unquoted(
    cs: &[char],
    pos: usize,
    first: &dyn Fn(char) -> bool,
    rest: &dyn Fn(char) -> bool,
    k: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if !cs.get(pos).copied().is_some_and(first) {
        return false;
    }
    let mut e = pos + 1;
    loop {
        if k(e) {
            return true;
        }
        if cs.get(e).copied().is_some_and(rest) {
            e += 1;
        } else {
            return false;
        }
    }
}

/// A member of the comma-terminated prefix of `_valueexp`.
fn m_list_item(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    m_quoted(cs, pos, '"', k)
        || m_quoted(cs, pos, '\'', k)
        || m_unquoted(
            cs,
            pos,
            &|c| !matches!(c, '\'' | '"' | ',' | '#'),
            &|c| !matches!(c, ',' | '#'),
            k,
        )
}

/// `\s*,\s*`. Only the longest leading run can end on the comma, but the
/// trailing run does have to be offered shortening.
fn m_comma(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let p = pos + ws_run(cs, pos);
    if cs.get(p) != Some(&',') {
        return false;
    }
    let q = p + 1;
    let mut r = q + ws_run(cs, q);
    loop {
        if k(r) {
            return true;
        }
        if r == q {
            return false;
        }
        r -= 1;
    }
}

/// `(?:ITEM \s*,\s*)*` -- greedy, so the longest run of complete list members
/// is offered first.
fn m_list_star(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    {
        let k = &mut *k;
        if m_list_item(cs, pos, &mut |after_item| {
            m_comma(cs, after_item, &mut |after_comma| {
                m_list_star(cs, after_comma, k)
            })
        }) {
            return true;
        }
    }
    k(pos)
}

/// The last, comma-less member of `_valueexp`, including the empty
/// alternative that only applies when the previous character is not a comma.
fn m_last_item(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    if m_quoted(cs, pos, '"', k) || m_quoted(cs, pos, '\'', k) {
        return true;
    }
    if m_unquoted(
        cs,
        pos,
        &|c| !matches!(c, '\'' | '"' | ',' | '#') && !c.is_whitespace(),
        &|c| c != ',',
        k,
    ) {
        return true;
    }
    let after_comma = pos > 0 && cs.get(pos - 1) == Some(&',');
    !after_comma && k(pos)
}

/// The groups `_valueexp` produces.
#[derive(Debug)]
struct ValueCaps {
    list_values: Option<String>,
    single: Option<String>,
    empty_list: bool,
    comment: String,
}

/// `_valueexp`.
fn m_valueexp(cs: &[char]) -> Option<ValueCaps> {
    let mut out: Option<ValueCaps> = None;
    {
        let out = &mut out;
        m_list_star(cs, 0, &mut |after_star| {
            let list_values = span(cs, 0, after_star);
            {
                let out = &mut *out;
                let list_values = list_values.clone();
                if m_last_item(cs, after_star, &mut |after_single| {
                    let single = span(cs, after_star, after_single);
                    m_tail(cs, after_single, &mut |comment| {
                        *out = Some(ValueCaps {
                            list_values: Some(list_values.clone()),
                            single: Some(single.clone()),
                            empty_list: false,
                            comment: comment.to_owned(),
                        });
                        true
                    })
                }) {
                    return true;
                }
            }
            m_tail(cs, after_star, &mut |comment| {
                *out = Some(ValueCaps {
                    list_values: Some(list_values.clone()),
                    single: None,
                    empty_list: false,
                    comment: comment.to_owned(),
                });
                true
            })
        });
    }
    if out.is_some() {
        return out;
    }
    if cs.first() == Some(&',') {
        m_tail(cs, 1, &mut |comment| {
            out = Some(ValueCaps {
                list_values: None,
                single: None,
                empty_list: true,
                comment: comment.to_owned(),
            });
            true
        });
    }
    out
}

/// `_listvalueexp` at one position, giving the end of group 1 and of the
/// whole match.
fn m_listvalue_at(cs: &[char], pos: usize) -> Option<(usize, usize)> {
    let mut found: Option<(usize, usize)> = None;
    {
        let found = &mut found;
        let take = |g1_end: usize, found: &mut Option<(usize, usize)>| -> bool {
            m_comma(cs, g1_end, &mut |end| {
                *found = Some((g1_end, end));
                true
            })
        };
        if m_quoted(cs, pos, '"', &mut |e| take(e, found))
            || m_quoted(cs, pos, '\'', &mut |e| take(e, found))
        {
            return found.take();
        }
        // `[^'",\#]?.*?`: the optional first character is greedy, then the
        // lazy run grows one character at a time.
        let optional = cs
            .get(pos)
            .copied()
            .is_some_and(|c| !matches!(c, '\'' | '"' | ',' | '#'));
        let starts = if optional {
            vec![pos + 1, pos]
        } else {
            vec![pos]
        };
        for start in starts {
            let mut e = start;
            loop {
                if take(e, found) {
                    return found.take();
                }
                if e >= cs.len() {
                    break;
                }
                e += 1;
            }
        }
    }
    found
}

/// `_listvalueexp.findall`.
fn findall_listvalues(cs: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= cs.len() {
        if let Some((g1_end, end)) = m_listvalue_at(cs, pos) {
            out.push(span(cs, pos, g1_end));
            pos = if end > pos { end } else { pos + 1 };
        } else {
            pos += 1;
        }
    }
    out
}

/// `_keyword`, giving the indentation, the raw key and the raw value.
fn m_keyword(cs: &[char]) -> Option<(String, String, String)> {
    let max_indent = ws_run(cs, 0);
    for indent_end in (0..=max_indent).rev() {
        let mut out: Option<(String, String, String)> = None;
        {
            let out = &mut out;
            let take =
                |key_end: usize, out: &mut Option<(String, String, String)>| -> bool {
                    let p = key_end + ws_run(cs, key_end);
                    if cs.get(p) != Some(&'=') {
                        return false;
                    }
                    let q = p + 1;
                    let value_start = q + ws_run(cs, q);
                    *out = Some((
                        span(cs, 0, indent_end),
                        span(cs, indent_end, key_end),
                        span(cs, value_start, cs.len()),
                    ));
                    true
                };
            let _ = m_quoted(cs, indent_end, '"', &mut |e| take(e, out))
                || m_quoted(cs, indent_end, '\'', &mut |e| take(e, out))
                || m_unquoted(
                    cs,
                    indent_end,
                    &|c| !matches!(c, '\'' | '"' | '='),
                    &|_| true,
                    &mut |e| take(e, out),
                );
        }
        if out.is_some() {
            return out;
        }
    }
    None
}

/// `(?:\[\s*)+`, greedy.
fn m_open(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    if cs.get(pos) != Some(&'[') {
        return false;
    }
    let q = pos + 1;
    let mut r = q + ws_run(cs, q);
    loop {
        if m_open(cs, r, k) || k(r) {
            return true;
        }
        if r == q {
            return false;
        }
        r -= 1;
    }
}

/// `(?:\s*\])+`, greedy.
fn m_close(cs: &[char], pos: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let p = pos + ws_run(cs, pos);
    if cs.get(p) != Some(&']') {
        return false;
    }
    let after = p + 1;
    m_close(cs, after, k) || k(after)
}

/// `"\s*\S.*?\s*"` -- a quoted section name with at least one non-space in it.
fn m_quoted_name(
    cs: &[char],
    pos: usize,
    quote: char,
    k: &mut dyn FnMut(usize) -> bool,
) -> bool {
    if cs.get(pos) != Some(&quote) {
        return false;
    }
    let first = pos + 1 + ws_run(cs, pos + 1);
    if cs.get(first).copied().is_none_or(char::is_whitespace) {
        return false;
    }
    let mut end = first + 1;
    loop {
        let close = end + ws_run(cs, end);
        if cs.get(close) == Some(&quote) && k(close + 1) {
            return true;
        }
        if end >= cs.len() {
            return false;
        }
        end += 1;
    }
}

/// The groups `_sectionmarker` produces.
#[derive(Debug)]
struct MarkerCaps {
    indent: String,
    open: String,
    name: String,
    close: String,
    comment: String,
}

/// `_sectionmarker`.
fn m_sectionmarker(cs: &[char]) -> Option<MarkerCaps> {
    let max_indent = ws_run(cs, 0);
    for indent_end in (0..=max_indent).rev() {
        let mut out: Option<MarkerCaps> = None;
        {
            let out = &mut out;
            m_open(cs, indent_end, &mut |after_open| {
                let take = |name_end: usize, out: &mut Option<MarkerCaps>| -> bool {
                    m_close(cs, name_end, &mut |after_close| {
                        m_tail(cs, after_close, &mut |comment| {
                            *out = Some(MarkerCaps {
                                indent: span(cs, 0, indent_end),
                                open: span(cs, indent_end, after_open),
                                name: span(cs, after_open, name_end),
                                close: span(cs, name_end, after_close),
                                comment: comment.to_owned(),
                            });
                            true
                        })
                    })
                };
                m_quoted_name(cs, after_open, '"', &mut |e| take(e, out))
                    || m_quoted_name(cs, after_open, '\'', &mut |e| take(e, out))
                    || m_unquoted(
                        cs,
                        after_open,
                        &|c| !matches!(c, '\'' | '"') && !c.is_whitespace(),
                        &|_| true,
                        &mut |e| take(e, out),
                    )
            });
        }
        if out.is_some() {
            return out;
        }
    }
    None
}

/// `^QQQ(.*?)QQQ\s*(#.*)?$`.
fn m_single_line_triple(cs: &[char], q: char) -> Option<(String, String)> {
    if !(cs.first() == Some(&q) && cs.get(1) == Some(&q) && cs.get(2) == Some(&q)) {
        return None;
    }
    let mut e = 3;
    loop {
        if cs.get(e) == Some(&q)
            && cs.get(e + 1) == Some(&q)
            && cs.get(e + 2) == Some(&q)
        {
            if let Some(comment) = tail_at(cs, e + 3) {
                return Some((span(cs, 3, e), comment));
            }
        }
        if e >= cs.len() {
            return None;
        }
        e += 1;
    }
}

/// `^(.*?)QQQ\s*(#.*)?$`.
fn m_multi_line_triple(cs: &[char], q: char) -> Option<(String, String)> {
    let mut e = 0;
    loop {
        if cs.get(e) == Some(&q)
            && cs.get(e + 1) == Some(&q)
            && cs.get(e + 2) == Some(&q)
        {
            if let Some(comment) = tail_at(cs, e + 3) {
                return Some((span(cs, 0, e), comment));
            }
        }
        if e >= cs.len() {
            return None;
        }
        e += 1;
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A stored value: either a string or a list of strings, which is all the
/// parser produces and all `_quote` knows how to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    /// A single value.
    Str(String),
    /// A comma-separated value.
    List(Vec<String>),
}

/// `__setitem__` of anything that is not a mapping, with `stringify=True`
/// doing the conversion `_quote` would otherwise do at write time.
fn scalar_from(value: &Value) -> Scalar {
    match value {
        Value::Array(items) => Scalar::List(items.iter().map(py_str).collect()),
        other => Scalar::Str(py_str(other)),
    }
}

/// Which quote `_quote` settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quot {
    None,
    Single,
    Double,
    TripleSingle,
    TripleDouble,
}

fn single_quote(value: &str) -> Result<Quot, Error> {
    let apostrophe = value.contains('\'');
    let quotation = value.contains('"');
    if apostrophe && quotation {
        Err(Error::CannotQuote(value.to_owned()))
    } else if quotation {
        Ok(Quot::Single)
    } else {
        Ok(Quot::Double)
    }
}

fn triple_quote(value: &str) -> Result<Quot, Error> {
    let quotations = value.contains("\"\"\"");
    let apostrophes = value.contains("'''");
    if quotations && apostrophes {
        return Err(Error::CannotQuote(value.to_owned()));
    }
    if quotations {
        Ok(Quot::TripleDouble)
    } else {
        Ok(Quot::TripleSingle)
    }
}

fn apply_quote(quot: Quot, value: &str) -> String {
    match quot {
        Quot::None => value.to_owned(),
        Quot::Single => format!("'{value}'"),
        Quot::Double => format!("\"{value}\""),
        Quot::TripleSingle => format!("'''{value}'''"),
        Quot::TripleDouble => format!("\"\"\"{value}\"\"\""),
    }
}

/// `_quote` for a string, with `list_values=True`, `stringify=True` and
/// `write_empty_values=False`.
fn quote_str(value: &str, multiline: bool) -> Result<String, Error> {
    if value.is_empty() {
        return Ok("\"\"".to_owned());
    }
    let apostrophe = value.contains('\'');
    let quotation = value.contains('"');
    let has_nl = value.contains('\n');
    let need_triple = multiline && ((apostrophe && quotation) || has_nl);
    let mut quot = if need_triple {
        triple_quote(value)?
    } else if has_nl {
        // Only reachable for a list member or a key, where triple quotes are
        // not on offer.
        return Err(Error::CannotQuote(value.to_owned()));
    } else {
        let clean_ends = value.chars().next().is_some_and(|c| !is_wspace_plus(c))
            && value
                .chars()
                .next_back()
                .is_some_and(|c| !is_wspace_plus(c));
        if clean_ends && !value.contains(',') {
            Quot::None
        } else {
            single_quote(value)?
        }
    };
    if quot == Quot::None && value.contains('#') {
        quot = single_quote(value)?;
    }
    Ok(apply_quote(quot, value))
}

/// `_quote` for a stored value, where a list is joined rather than quoted.
fn quote_scalar(value: &Scalar) -> Result<String, Error> {
    match value {
        Scalar::Str(text) => quote_str(text, true),
        Scalar::List(items) => match items.split_first() {
            None => Ok(",".to_owned()),
            Some((only, [])) => Ok(format!("{},", quote_str(only, false)?)),
            _ => {
                let parts: Result<Vec<_>, _> =
                    items.iter().map(|item| quote_str(item, false)).collect();
                Ok(parts?.join(", "))
            }
        },
    }
}

/// `_unquote`.
fn unquote(value: &str) -> Result<String, ()> {
    if value.is_empty() {
        return Err(());
    }
    let first = value.chars().next();
    let last = value.chars().next_back();
    if first == last && value.chars().count() >= 2 && matches!(first, Some('"' | '\''))
    {
        let mut chars = value.chars();
        chars.next();
        chars.next_back();
        return Ok(chars.as_str().to_owned());
    }
    // A one-character value whose first and last character are the same quote
    // is stripped to nothing by Python's `value[1:-1]`.
    if value.chars().count() == 1 && matches!(first, Some('"' | '\'')) {
        return Ok(String::new());
    }
    Ok(value.to_owned())
}

/// `_handle_value`, giving the stored value and the inline comment.
fn handle_value(value: &str) -> Result<(Scalar, String), ()> {
    let cs: Vec<char> = value.chars().collect();
    let caps = m_valueexp(&cs).ok_or(())?;
    if caps.list_values.as_deref() == Some("") && caps.single.is_none() {
        return Err(());
    }
    if caps.empty_list {
        return Ok((Scalar::List(Vec::new()), caps.comment));
    }
    let list_values = caps.list_values.clone().unwrap_or_default();
    let mut single = caps.single.clone();
    if let Some(text) = single.clone() {
        if !list_values.is_empty() && text.is_empty() {
            single = None;
        } else {
            let source = if text.is_empty() {
                "\"\"".to_owned()
            } else {
                text
            };
            single = Some(unquote(&source)?);
        }
    }
    if list_values.is_empty() {
        return Ok((Scalar::Str(single.unwrap_or_default()), caps.comment));
    }
    let members: Vec<char> = list_values.chars().collect();
    let mut items = Vec::new();
    for raw in findall_listvalues(&members) {
        items.push(unquote(&raw)?);
    }
    if let Some(text) = single {
        items.push(text);
    }
    Ok((Scalar::List(items), caps.comment))
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// A stored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Scalar(Scalar),
    Section(Section),
}

/// A `Section`: the ordered scalar names, the ordered section names, the
/// values and the comments attached to each name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Section {
    depth: usize,
    scalars: Vec<String>,
    sections: Vec<String>,
    items: Vec<(String, Item)>,
    comments: Vec<(String, Vec<String>)>,
    inline: Vec<(String, String)>,
}

impl Section {
    fn new(depth: usize) -> Self {
        Self {
            depth,
            ..Self::default()
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.items.iter().any(|(name, _)| name == key)
    }

    fn put(&mut self, key: &str, item: Item) {
        if let Some(slot) = self
            .items
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, item)| item)
        {
            *slot = item;
        } else {
            self.items.push((key.to_owned(), item));
        }
    }

    fn get(&self, key: &str) -> Option<&Item> {
        self.items
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, item)| item)
    }

    fn get_section_mut(&mut self, key: &str) -> Option<&mut Self> {
        self.items
            .iter_mut()
            .find(|(name, _)| name == key)
            .and_then(|(_, item)| match item {
                Item::Section(section) => Some(section),
                Item::Scalar(_) => None,
            })
    }

    fn comment_slot(&mut self, key: &str) {
        if !self.comments.iter().any(|(name, _)| name == key) {
            self.comments.push((key.to_owned(), Vec::new()));
            self.inline.push((key.to_owned(), String::new()));
        }
    }

    fn set_comments(&mut self, key: &str, lines: Vec<String>, inline: &str) {
        self.comment_slot(key);
        if let Some(slot) = self
            .comments
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, lines)| lines)
        {
            *slot = lines;
        }
        if let Some(slot) = self
            .inline
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|(_, text)| text)
        {
            inline.clone_into(slot);
        }
    }

    fn comments_for(&self, key: &str) -> &[String] {
        self.comments
            .iter()
            .find(|(name, _)| name == key)
            .map_or(&[][..], |(_, lines)| lines.as_slice())
    }

    fn inline_for(&self, key: &str) -> &str {
        self.inline
            .iter()
            .find(|(name, _)| name == key)
            .map_or("", |(_, text)| text.as_str())
    }

    /// `__setitem__`. A mapping becomes a sub-section, anything else a scalar,
    /// and a name that is already present keeps the place it had in whichever
    /// of the two lists it landed in first.
    pub fn set(&mut self, key: &str, value: &Value) {
        self.comment_slot(key);
        if let Value::Object(map) = value {
            if !self.contains(key) {
                self.sections.push(key.to_owned());
            }
            let mut sub = Self::new(self.depth + 1);
            for (name, item) in map {
                sub.set(name, item);
            }
            self.put(key, Item::Section(sub));
        } else {
            if !self.contains(key) {
                self.scalars.push(key.to_owned());
            }
            self.put(key, Item::Scalar(scalar_from(value)));
        }
    }

    /// `__setitem__` with a value that is already a Python `str`, which is how
    /// `cc_mcollective` stores the `str(cfg)` of a non-string, non-mapping.
    pub fn set_str(&mut self, key: &str, value: &str) {
        self.set(key, &Value::String(value.to_owned()));
    }

    /// The `sections` list, which is what `cc_mcollective` tests membership
    /// against before it creates one.
    #[must_use]
    pub fn section_names(&self) -> &[String] {
        &self.sections
    }

    /// The value stored under `key`, when it is a scalar.
    #[must_use]
    pub fn scalar(&self, key: &str) -> Option<&Scalar> {
        match self.get(key)? {
            Item::Scalar(value) => Some(value),
            Item::Section(_) => None,
        }
    }
}

/// A parsed config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigObj {
    root: Section,
    initial_comment: Vec<String>,
    final_comment: Vec<String>,
    indent_type: Option<String>,
    newlines: Option<String>,
    bom: bool,
}

impl ConfigObj {
    /// `ConfigObj()` with no file behind it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The root section, for the `mcollective_config[name][option] = value`
    /// path.
    pub fn root_mut(&mut self) -> &mut Section {
        &mut self.root
    }

    /// The sub-section stored under `key`, when there is one.
    pub fn section_mut(&mut self, key: &str) -> Option<&mut Section> {
        self.root.get_section_mut(key)
    }

    /// `__setitem__` on the root.
    pub fn set(&mut self, key: &str, value: &Value) {
        self.root.set(key, value);
    }

    /// `__setitem__` on the root with a value that is already a string.
    pub fn set_str(&mut self, key: &str, value: &str) {
        self.root.set_str(key, value);
    }

    /// The root's `sections` list.
    #[must_use]
    pub fn section_names(&self) -> &[String] {
        self.root.section_names()
    }

    /// `ConfigObj(io.BytesIO(data))`.
    ///
    /// # Errors
    /// The decode error, or the first parse error, or the summary error when
    /// there was more than one.
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        let mut this = Self::new();
        if data.is_empty() {
            this.indent_type = Some(String::new());
            return Ok(this);
        }
        let (text, bom) = decode_utf8(data)?;
        this.bom = bom;
        // The newline is the ending of the first line that has one.
        for line in split_keepends(&text) {
            if !(line.ends_with('\n') || line.ends_with('\r')) {
                continue;
            }
            for end in ["\r\n", "\n", "\r"] {
                if line.ends_with(end) {
                    this.newlines = Some(end.to_owned());
                    break;
                }
            }
            break;
        }
        let lines: Vec<String> = split_keepends(&text)
            .into_iter()
            .map(|line| line.trim_end_matches(['\r', '\n']).to_owned())
            .collect();
        this.parse_lines(&lines)?;
        Ok(this)
    }

    /// The section-marker branch of `_parse`, giving the path of the section
    /// that lines after it belong to.
    fn parse_marker(
        &mut self,
        caps: &MarkerCaps,
        lineno: usize,
        stack: &[String],
        comment_list: &mut Vec<String>,
    ) -> Result<Vec<String>, Error> {
        if !caps.indent.is_empty() && self.indent_type.is_none() {
            self.indent_type = Some(caps.indent.clone());
        }
        let cur_depth = caps.open.matches('[').count();
        if cur_depth != caps.close.matches(']').count() {
            return Err(Error::Nesting("Cannot compute the section depth", lineno));
        }
        // `_match_depth` walks up to the ancestor at `cur_depth` and takes its
        // parent, which is the same as dropping the last `cur_depth` names off
        // the current path -- and the root when there are none left.
        let parent_path: Vec<String> = if cur_depth <= stack.len() {
            stack
                .get(0..cur_depth.saturating_sub(1))
                .unwrap_or_default()
                .to_vec()
        } else if cur_depth == stack.len() + 1 {
            stack.to_vec()
        } else {
            return Err(Error::Nesting("Section too nested", lineno));
        };
        let name = unquote(&caps.name).unwrap_or_else(|()| caps.name.clone());
        let Some(parent) = section_at_mut(&mut self.root, &parent_path) else {
            return Err(Error::Nesting("Cannot compute nesting level", lineno));
        };
        if parent.contains(&name) {
            return Err(Error::Duplicate("Duplicate section name", lineno));
        }
        parent.sections.push(name.clone());
        parent.put(&name, Item::Section(Section::new(cur_depth)));
        parent.set_comments(&name, std::mem::take(comment_list), &caps.comment);
        let mut next = parent_path;
        next.push(name);
        Ok(next)
    }

    /// The `key = value` branch of `_parse`, giving the index to carry on
    /// from -- which a triple-quoted value moves past the lines it spans.
    fn parse_keyword(
        &mut self,
        line: &str,
        lines: &[String],
        cursor: usize,
        stack: &[String],
        comment_list: &mut Vec<String>,
    ) -> Result<usize, Error> {
        let lineno = cursor + 1;
        let cs: Vec<char> = line.chars().collect();
        let Some((indent, raw_key, raw_value)) = m_keyword(&cs) else {
            return Err(Error::Parse(
                format!(
                    "Invalid line ({}) (matched as neither section nor keyword)",
                    crate::repr::repr_str(line)
                ),
                lineno,
            ));
        };
        if !indent.is_empty() && self.indent_type.is_none() {
            self.indent_type = Some(indent);
        }
        let leader: String = raw_value.chars().take(3).collect();
        let mut next = cursor + 1;
        let (value, comment) = if leader == "\"\"\"" || leader == "'''" {
            let quote = if leader == "\"\"\"" { '"' } else { '\'' };
            let Some((text, comment, end)) =
                multiline(&raw_value, lines, cursor, quote)
            else {
                return Err(Error::Parse(
                    "Parse error in multiline value".into(),
                    lineno,
                ));
            };
            next = end + 1;
            (Scalar::Str(text), comment)
        } else {
            let Ok(pair) = handle_value(&raw_value) else {
                return Err(Error::Parse("Parse error in value".into(), lineno));
            };
            pair
        };
        let key = unquote(&raw_key).unwrap_or_else(|()| raw_key.clone());
        let Some(section) = section_at_mut(&mut self.root, stack) else {
            return Err(Error::Nesting("Cannot compute nesting level", lineno));
        };
        if section.contains(&key) {
            return Err(Error::Duplicate("Duplicate keyword name", lineno));
        }
        section.scalars.push(key.clone());
        section.put(&key, Item::Scalar(value));
        section.set_comments(&key, std::mem::take(comment_list), &comment);
        Ok(next)
    }

    /// `_parse`.
    fn parse_lines(&mut self, lines: &[String]) -> Result<(), Error> {
        let mut errors: Vec<Error> = Vec::new();
        let mut comment_list: Vec<String> = Vec::new();
        let mut done_start = false;
        let mut reset_comment = false;
        // The path from the root to the section being filled.
        let mut stack: Vec<String> = Vec::new();

        let mut index = 0;
        while let Some(line) = lines.get(index) {
            let cursor = index;
            index += 1;
            if reset_comment {
                comment_list = Vec::new();
            }
            let stripped = line.trim();
            if stripped.is_empty() || stripped.starts_with('#') {
                reset_comment = false;
                comment_list.push(line.clone());
                continue;
            }
            if !done_start {
                self.initial_comment = std::mem::take(&mut comment_list);
                done_start = true;
            }
            reset_comment = true;

            let cs: Vec<char> = line.chars().collect();
            if let Some(caps) = m_sectionmarker(&cs) {
                match self.parse_marker(&caps, cursor + 1, &stack, &mut comment_list) {
                    Ok(next) => stack = next,
                    Err(error) => errors.push(error),
                }
                continue;
            }
            match self.parse_keyword(line, lines, cursor, &stack, &mut comment_list) {
                Ok(next) => index = next,
                Err(error) => errors.push(error),
            }
        }

        if self.indent_type.is_none() {
            self.indent_type = Some(String::new());
        }
        if self.root.items.is_empty() && self.initial_comment.is_empty() {
            self.initial_comment = comment_list;
        } else if !reset_comment {
            self.final_comment = comment_list;
        }

        match errors.split_first() {
            None => Ok(()),
            Some((first, [])) => Err(first.clone()),
            Some((first, _)) => Err(Error::Several(error_line(first))),
        }
    }

    /// `write`, giving the bytes the caller would hand to `util.write_file`.
    ///
    /// # Errors
    /// A value that cannot be quoted, or a non-ASCII character in the result.
    pub fn write(&mut self) -> Result<Vec<u8>, Error> {
        if self.indent_type.is_none() {
            self.indent_type = Some(DEFAULT_INDENT_TYPE.to_owned());
        }
        let indent_type = self.indent_type.clone().unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        for line in &self.initial_comment {
            out.push(comment_line(line));
        }
        write_section(&self.root, &indent_type, &mut out)?;
        for line in &self.final_comment {
            out.push(comment_line(line));
        }
        let newline = self.newlines.clone().unwrap_or_else(|| "\n".to_owned());
        let mut text = out.join(&newline);
        if !text.ends_with(&newline) {
            text.push_str(&newline);
        }
        encode_ascii(&text, self.bom)
    }
}

/// One of `initial_comment`/`final_comment`, which get a `# ` only when they
/// have content that is not already a comment.
fn comment_line(line: &str) -> String {
    let stripped = line.trim();
    if !stripped.is_empty() && !stripped.starts_with('#') {
        format!("# {line}")
    } else {
        line.to_owned()
    }
}

/// `_handle_comment`.
fn handle_comment(indent_type: &str, comment: &str) -> String {
    if comment.is_empty() {
        return String::new();
    }
    let mut start = indent_type.to_owned();
    if !comment.starts_with('#') {
        start.push_str(" # ");
    }
    start + comment
}

/// The body of `write`, for one section.
fn write_section(
    section: &Section,
    indent_type: &str,
    out: &mut Vec<String>,
) -> Result<(), Error> {
    let indent_string = indent_type.repeat(section.depth);
    let order: Vec<String> = section
        .scalars
        .iter()
        .chain(section.sections.iter())
        .cloned()
        .collect();
    for entry in order {
        for line in section.comments_for(&entry) {
            let trimmed = line.trim_start();
            let text = if !trimmed.is_empty() && !trimmed.starts_with('#') {
                format!("# {trimmed}")
            } else {
                trimmed.to_owned()
            };
            out.push(format!("{indent_string}{text}"));
        }
        let comment = handle_comment(indent_type, section.inline_for(&entry));
        match section.get(&entry) {
            Some(Item::Section(sub)) => {
                let open = "[".repeat(sub.depth);
                let close = "]".repeat(sub.depth);
                let name = quote_str(&entry, false)?;
                out.push(format!("{indent_string}{open}{name}{close}{comment}"));
                write_section(sub, indent_type, out)?;
            }
            Some(Item::Scalar(value)) => {
                let name = quote_str(&entry, false)?;
                let text = quote_scalar(value)?;
                out.push(format!("{indent_string}{name} = {text}{comment}"));
            }
            None => {}
        }
    }
    Ok(())
}

/// Follow a path of section names from the root.
fn section_at_mut<'a>(
    root: &'a mut Section,
    path: &[String],
) -> Option<&'a mut Section> {
    let mut here = root;
    for name in path {
        here = here.get_section_mut(name)?;
    }
    Some(here)
}

/// The line number an error carries, for the summary message.
fn error_line(error: &Error) -> usize {
    match error {
        Error::Nesting(_, line) | Error::Duplicate(_, line) | Error::Parse(_, line) => {
            *line
        }
        _ => 0,
    }
}

/// `_multiline`, giving the value, the comment and the index of the last line
/// it consumed.
fn multiline(
    value: &str,
    lines: &[String],
    index: usize,
    quote: char,
) -> Option<(String, String, usize)> {
    let cs: Vec<char> = value.chars().collect();
    if let Some((text, comment)) = m_single_line_triple(&cs, quote) {
        return Some((text, comment, index));
    }
    let mut newvalue: String = value.chars().skip(3).collect();
    let quot: String = std::iter::repeat_n(quote, 3).collect();
    if newvalue.contains(&quot) {
        return None;
    }
    let mut cur = index;
    let maxline = lines.len().checked_sub(1)?;
    let closing = loop {
        if cur >= maxline {
            return None;
        }
        cur += 1;
        newvalue.push('\n');
        let line = lines.get(cur)?;
        if line.contains(&quot) {
            break line.clone();
        }
        newvalue.push_str(line);
    };
    let tail: Vec<char> = closing.chars().collect();
    let (text, comment) = m_multi_line_triple(&tail, quote)?;
    Some((newvalue + &text, comment, cur))
}

/// `str.splitlines(True)` for the endings `configobj` cares about.
fn split_keepends(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        if c == '\n' {
            out.push(std::mem::take(&mut current));
        } else if c == '\r' {
            if chars.peek() == Some(&'\n') {
                current.push('\n');
                chars.next();
            }
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `_handle_bom` plus the decode, for the UTF-8 case the module can produce.
fn decode_utf8(data: &[u8]) -> Result<(String, bool), Error> {
    let (body, bom) = match data.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        Some(rest) => (rest, true),
        None => (data, false),
    };
    match std::str::from_utf8(body) {
        Ok(text) => Ok((text.to_owned(), bom)),
        Err(error) => {
            let offset = if bom { 3 } else { 0 };
            let start = error.valid_up_to();
            let lead = body.get(start).copied();
            let (len, reason) = match error.error_len() {
                Some(len) => {
                    let reason = match lead {
                        Some(byte) if (0x80..=0xc1).contains(&byte) || byte >= 0xf5 => {
                            "invalid start byte"
                        }
                        _ => "invalid continuation byte",
                    };
                    (len, reason)
                }
                None => (body.len() - start, "unexpected end of data"),
            };
            Err(Error::Decode {
                byte: if len == 1 { lead } else { None },
                start: start + offset,
                end: start + offset + len - 1,
                reason,
            })
        }
    }
}

/// `output.encode('ascii')` plus the BOM the write path puts back.
fn encode_ascii(text: &str, bom: bool) -> Result<Vec<u8>, Error> {
    if let Some((index, ch)) = text.char_indices().find(|(_, c)| !c.is_ascii()) {
        // Python counts in characters, not bytes.
        let start = text
            .get(0..index)
            .map_or(0, |prefix| prefix.chars().count());
        let run = text
            .chars()
            .skip(start)
            .take_while(|c| !c.is_ascii())
            .count();
        return Err(Error::Encode {
            ch: if run == 1 { Some(ch) } else { None },
            start,
            end: start + run - 1,
        });
    }
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    out.extend_from_slice(text.as_bytes());
    Ok(out)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(source: &str) -> String {
        let mut config = ConfigObj::parse(source.as_bytes()).unwrap();
        String::from_utf8(config.write().unwrap()).unwrap()
    }

    #[test]
    fn spacing_is_normalised_and_quotes_are_dropped() {
        assert_eq!(roundtrip("a=1\n"), "a = 1\n");
        assert_eq!(roundtrip("a = \"x\"\n"), "a = x\n");
        assert_eq!(roundtrip("a = 'x'\n"), "a = x\n");
        assert_eq!(roundtrip("a=1"), "a = 1\n");
    }

    #[test]
    fn a_comma_makes_a_list() {
        assert_eq!(roundtrip("a = 1, 2, 3\n"), "a = 1, 2, 3\n");
        assert_eq!(roundtrip("a = 1,\n"), "a = 1,\n");
        assert_eq!(roundtrip("a = ,\n"), "a = ,\n");
    }

    #[test]
    fn an_inline_comment_loses_the_space_before_it() {
        assert_eq!(roundtrip("a = x # c\n"), "a = x# c\n");
        assert_eq!(roundtrip("a = x, y # c\n"), "a = x, y# c\n");
    }

    #[test]
    fn the_first_indent_sets_the_indent_for_the_file() {
        assert_eq!(roundtrip("  a = 1\n"), "a = 1\n");
        assert_eq!(roundtrip("[a]\n  b = 1\n"), "[a]\n  b = 1\n");
        assert_eq!(
            roundtrip("# lead\n[s] # sc\n  x = 2 # xc\n  [[t]] # tc\n    y = 3\n"),
            "# lead\n[s]  # sc\n  x = 2  # xc\n  [[t]]  # tc\n    y = 3\n"
        );
    }

    #[test]
    fn triple_quotes_come_back_single() {
        assert_eq!(roundtrip("a = \"\"\"x\ny\"\"\"\n"), "a = '''x\ny'''\n");
    }

    #[test]
    fn nesting_and_its_limits() {
        assert_eq!(roundtrip("[a]\n[[b]]\nx=1\n"), "[a]\n[[b]]\nx = 1\n");
        let error = ConfigObj::parse(b"[a]\n[[[c]]]\nx=1\n").unwrap_err();
        assert_eq!(error.kind(), "NestingError");
        assert_eq!(error.to_string(), "Section too nested at line 2.");
    }

    #[test]
    fn duplicates_and_bad_lines() {
        let error = ConfigObj::parse(b"[a]\n[a]\n").unwrap_err();
        assert_eq!(error.to_string(), "Duplicate section name at line 2.");
        let error = ConfigObj::parse(b"a=1\na=2\n").unwrap_err();
        assert_eq!(error.to_string(), "Duplicate keyword name at line 2.");
        let error = ConfigObj::parse(b"garbage\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid line ('garbage') (matched as neither section nor keyword) at line 1."
        );
        let error = ConfigObj::parse(b"bad1\nbad2\n").unwrap_err();
        assert_eq!(error.kind(), "ConfigObjError");
        assert_eq!(
            error.to_string(),
            "Parsing failed with several errors.\nFirst error at line 1."
        );
    }

    #[test]
    fn comments_at_both_ends() {
        assert_eq!(roundtrip("a = 1\n# trailing\n"), "a = 1\n# trailing\n");
        assert_eq!(roundtrip("# only comment\n"), "# only comment\n");
    }

    #[test]
    fn an_empty_config_is_one_newline() {
        assert_eq!(roundtrip(""), "\n");
        let mut config = ConfigObj::new();
        assert_eq!(String::from_utf8(config.write().unwrap()).unwrap(), "\n");
    }

    #[test]
    fn line_endings_survive() {
        assert_eq!(roundtrip("a = 1\r\nb = 2\r\n"), "a = 1\r\nb = 2\r\n");
    }

    #[test]
    fn values_are_stringified_and_quoted_as_needed() {
        let mut config = ConfigObj::new();
        config.set("k", &json!(["x", "y"]));
        config.set("n", &json!(5));
        config.set("d", &Value::Null);
        config.set("e", &json!(""));
        assert_eq!(
            String::from_utf8(config.write().unwrap()).unwrap(),
            "k = x, y\nn = 5\nd = None\ne = \"\"\n"
        );
    }

    #[test]
    fn quoting_rules() {
        let cases = [
            ("a b", "k = a b\n"),
            ("a,b", "k = \"a,b\"\n"),
            ("a#b", "k = \"a#b\"\n"),
            ("a\"b", "k = a\"b\n"),
            ("a'b", "k = a'b\n"),
            ("  pad  ", "k = \"  pad  \"\n"),
            ("a\nb", "k = '''a\nb'''\n"),
            ("#lead", "k = \"#lead\"\n"),
            ("=x", "k = =x\n"),
            ("a]b", "k = a]b\n"),
        ];
        for (value, expected) in cases {
            let mut config = ConfigObj::new();
            config.set_str("k", value);
            assert_eq!(
                String::from_utf8(config.write().unwrap()).unwrap(),
                expected,
                "for {value:?}"
            );
        }
    }

    #[test]
    fn scalars_are_written_before_sections() {
        let mut config = ConfigObj::parse(b"[s]\nx = 1\n").unwrap();
        config.set_str("top", "v");
        assert_eq!(
            String::from_utf8(config.write().unwrap()).unwrap(),
            "top = v\n[s]\nx = 1\n"
        );
    }

    #[test]
    fn a_scalar_overwritten_with_a_mapping_keeps_its_place() {
        let mut config = ConfigObj::parse(b"top = 1\n[s]\nx = 2\n").unwrap();
        config.set("top", &json!({"a": "b"}));
        assert_eq!(
            String::from_utf8(config.write().unwrap()).unwrap(),
            "[top]\na = b\n[s]\nx = 2\n"
        );
        assert_eq!(config.section_names(), ["s"]);
    }

    #[test]
    fn a_new_nested_mapping_is_not_indented() {
        let mut config = ConfigObj::new();
        config.set("a", &json!({"b": {"c": "d"}}));
        assert_eq!(
            String::from_utf8(config.write().unwrap()).unwrap(),
            "[a]\n[[b]]\nc = d\n"
        );
    }

    #[test]
    fn a_utf8_bom_survives_and_other_bytes_do_not() {
        let mut config = ConfigObj::parse(b"\xef\xbb\xbfa = 1\n").unwrap();
        assert_eq!(config.write().unwrap(), b"\xef\xbb\xbfa = 1\n");
        let error = ConfigObj::parse(b"a = \xff\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "'utf-8' codec can't decode byte 0xff in position 4: invalid start byte"
        );
        let error = ConfigObj::parse(b"a = \xe9\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "'utf-8' codec can't decode byte 0xe9 in position 4: invalid continuation byte"
        );
        let error = ConfigObj::parse(b"a = \xf0\x9f\x98").unwrap_err();
        assert_eq!(
            error.to_string(),
            "'utf-8' codec can't decode bytes in position 4-6: unexpected end of data"
        );
    }

    #[test]
    fn a_non_ascii_value_parses_and_then_fails_to_write() {
        let mut config = ConfigObj::parse("a = \u{e9}\n".as_bytes()).unwrap();
        let error = config.write().unwrap_err();
        assert_eq!(error.kind(), "UnicodeEncodeError");
        assert_eq!(
            error.to_string(),
            "'ascii' codec can't encode character '\\xe9' in position 4: \
             ordinal not in range(128)"
        );
        let mut config = ConfigObj::parse("a = \u{e9}\u{e9}\n".as_bytes()).unwrap();
        assert_eq!(
            config.write().unwrap_err().to_string(),
            "'ascii' codec can't encode characters in position 4-5: \
             ordinal not in range(128)"
        );
    }

    #[test]
    fn a_quoted_key_loses_its_quotes() {
        assert_eq!(roundtrip("\"k k\" = 1\n"), "k k = 1\n");
        assert_eq!(roundtrip("[ a ]\nx=1\n"), "[a]\nx = 1\n");
    }

    #[test]
    fn bad_values_are_parse_errors() {
        for (source, expected) in [
            ("a = \"unclosed\n", "Parse error in value at line 1."),
            (
                "a = \"\"\"x\n",
                "Parse error in multiline value at line 1.",
            ),
            (
                "= 5\n",
                "Invalid line ('= 5') (matched as neither section nor keyword) at line 1.",
            ),
            (
                "[]\n",
                "Invalid line ('[]') (matched as neither section nor keyword) at line 1.",
            ),
        ] {
            let error = ConfigObj::parse(source.as_bytes()).unwrap_err();
            assert_eq!(error.to_string(), expected, "for {source:?}");
        }
    }
}
