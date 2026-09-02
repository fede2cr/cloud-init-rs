//! `PyYAML`'s block emitter, configured the way `cloudinit.safeyaml.dumps` is.
//!
//! Ported from `yaml/emitter.py`. A general-purpose Rust YAML serialiser cannot
//! stand in for it: upstream dumps with `indent=4` and `width=80`, so every
//! nested collection and every scalar longer than the line budget is laid out
//! differently. `status --format=yaml` looks identical to `serde_yaml_ng` output
//! only while the status is empty; the moment a real error message or a
//! populated `recoverable_errors` appears, the two diverge.
//!
//! The emitter state mirrors upstream's (`column`, `indent`, `whitespace`,
//! `indention`) because the folding and indentation decisions are expressed in
//! terms of it, and approximating them is how the divergence started.

use ci_config::yaml::plain_resolves_to_str;
use serde_json::Value;

/// `Emitter.best_indent`, from `safeyaml.dumps(indent=4)`.
const BEST_INDENT: usize = 4;
/// `Emitter.best_width`. Upstream passes no `width`, so `PyYAML` defaults to 80.
const BEST_WIDTH: usize = 80;

/// Characters `PyYAML` escapes by name inside a double-quoted scalar.
fn escape_replacement(ch: char) -> Option<char> {
    Some(match ch {
        '\0' => '0',
        '\u{7}' => 'a',
        '\u{8}' => 'b',
        '\t' => 't',
        '\n' => 'n',
        '\u{b}' => 'v',
        '\u{c}' => 'f',
        '\r' => 'r',
        '\u{1b}' => 'e',
        '"' => '"',
        '\\' => '\\',
        '\u{85}' => 'N',
        '\u{a0}' => '_',
        '\u{2028}' => 'L',
        '\u{2029}' => 'P',
        _ => return None,
    })
}

fn is_break(ch: char) -> bool {
    matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `safeyaml.dumps()`: sorted keys, `---`/`...` markers, indent 4, block style.
///
/// The result ends with a newline after `...`; upstream `print()`s it, which
/// adds one more.
#[must_use]
pub fn dumps(value: &Value) -> String {
    let mut emitter = Emitter {
        out: String::new(),
        column: 0,
        indent: None,
        whitespace: true,
        indention: true,
    };
    emitter.write_indicator("---", true, false, true);
    emitter.emit_node(value, false, false);
    emitter.write_indent();
    emitter.write_indicator("...", true, false, false);
    emitter.write_indent();
    emitter.out
}

#[derive(Debug)]
struct Emitter {
    out: String,
    column: usize,
    indent: Option<usize>,
    whitespace: bool,
    indention: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Single,
    Double,
}

impl Emitter {
    fn write(&mut self, data: &str) {
        self.column += data.chars().count();
        self.out.push_str(data);
    }

    fn write_line_break(&mut self) {
        self.out.push('\n');
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention
            || self.column > indent
            || (self.column == indent && !self.whitespace)
        {
            self.write_line_break();
        }
        if self.column < indent {
            self.whitespace = true;
            let pad = indent.saturating_sub(self.column);
            for _ in 0..pad {
                self.out.push(' ');
            }
            self.column = indent;
        }
    }

    fn write_indicator(
        &mut self,
        indicator: &str,
        need_whitespace: bool,
        whitespace: bool,
        indention: bool,
    ) {
        if !self.whitespace && need_whitespace {
            self.out.push(' ');
            self.column += 1;
        }
        self.write(indicator);
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
    }

    /// Returns the previous indent, which the caller restores. Upstream keeps a
    /// stack; the recursion here already is one.
    fn increase_indent(&mut self, flow: bool, indentless: bool) -> Option<usize> {
        let saved = self.indent;
        match self.indent {
            None => self.indent = Some(if flow { BEST_INDENT } else { 0 }),
            Some(current) if !indentless => self.indent = Some(current + BEST_INDENT),
            Some(_) => {}
        }
        saved
    }

    /// `expect_node`. `mapping_context` marks a mapping *value*, which is the
    /// only thing that makes a nested block sequence indentless.
    fn emit_node(&mut self, value: &Value, mapping_context: bool, simple_key: bool) {
        match value {
            Value::Object(map) if map.is_empty() => {
                self.write_indicator("{", true, true, true);
                self.write_indicator("}", false, false, false);
            }
            Value::Array(items) if items.is_empty() => {
                self.write_indicator("[", true, true, true);
                self.write_indicator("]", false, false, false);
            }
            Value::Object(map) => self.emit_block_mapping(map),
            Value::Array(items) => self.emit_block_sequence(items, mapping_context),
            scalar => {
                let (text, implicit_plain) = scalar_text(scalar);
                self.emit_scalar(&text, implicit_plain, simple_key);
            }
        }
    }

    fn emit_scalar(&mut self, text: &str, implicit_plain: bool, simple_key: bool) {
        let saved = self.increase_indent(true, false);
        let analysis = analyze_scalar(text);
        let style = choose_style(&analysis, implicit_plain, simple_key);
        let split = !simple_key;
        match style {
            Style::Plain => self.write_plain(text, split),
            Style::Single => self.write_single_quoted(text, split),
            Style::Double => self.write_double_quoted(text, split),
        }
        self.indent = saved;
    }

    fn emit_block_mapping(&mut self, map: &serde_json::Map<String, Value>) {
        let saved = self.increase_indent(false, false);
        // `SafeRepresenter.represent_mapping` sorts; Rust's `str` ordering is by
        // code point, which is what Python compares too.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let Some(value) = map.get(key) else { continue };
            self.write_indent();
            if is_simple_key(key) {
                self.emit_scalar(key, plain_resolves_to_str(key), true);
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, true);
                self.emit_scalar(key, plain_resolves_to_str(key), false);
                self.write_indent();
                self.write_indicator(":", true, false, true);
            }
            self.emit_node(value, true, false);
        }
        self.indent = saved;
    }

    fn emit_block_sequence(&mut self, items: &[Value], mapping_context: bool) {
        let indentless = mapping_context && !self.indention;
        let saved = self.increase_indent(false, indentless);
        for item in items {
            self.write_indent();
            self.write_indicator("-", true, false, true);
            self.emit_node(item, false, false);
        }
        self.indent = saved;
    }

    fn write_plain(&mut self, text: &str, split: bool) {
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.out.push(' ');
            self.column += 1;
        }
        self.whitespace = false;
        self.indention = false;
        // Plain scalars never carry line breaks: `analyze_scalar` refuses the
        // style for anything multiline, so only the space run needs folding.
        let chars: Vec<char> = text.chars().collect();
        let mut spaces = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= chars.len() {
            let ch = chars.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        let data: String = slice(&chars, start, end);
                        self.write(&data);
                    }
                    start = end;
                }
            } else if ch.is_none() || ch == Some(' ') {
                let data: String = slice(&chars, start, end);
                self.write(&data);
                start = end;
            }
            if let Some(ch) = ch {
                spaces = ch == ' ';
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("'", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let mut spaces = false;
        let mut breaks = false;
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= chars.len() {
            let ch = chars.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end
                        && self.column > BEST_WIDTH
                        && split
                        && start != 0
                        && end != chars.len()
                    {
                        self.write_indent();
                    } else {
                        let data = slice(&chars, start, end);
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    // A leading break is doubled: a lone newline folds to a
                    // space on load, so the literal one needs an escape line.
                    if chars.get(start) == Some(&'\n') {
                        self.write_line_break();
                    }
                    for _ in slice_chars(&chars, start, end) {
                        self.write_line_break();
                    }
                    self.write_indent();
                    start = end;
                }
            } else if start < end
                && (ch.is_none()
                    || ch == Some(' ')
                    || ch.is_some_and(is_break)
                    || ch == Some('\''))
            {
                let data = slice(&chars, start, end);
                self.write(&data);
                start = end;
            }
            if ch == Some('\'') {
                self.write("''");
                start = end + 1;
            }
            if let Some(ch) = ch {
                spaces = ch == ' ';
                breaks = is_break(ch);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("\"", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let mut start = 0usize;
        let mut end = 0usize;
        while end <= chars.len() {
            let ch = chars.get(end).copied();
            let needs_escape = match ch {
                None => true,
                Some(c) => {
                    matches!(
                        c,
                        '"' | '\\' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}'
                    ) || !('\x20'..='\x7e').contains(&c)
                }
            };
            if needs_escape {
                if start < end {
                    let data = slice(&chars, start, end);
                    self.write(&data);
                    start = end;
                }
                if let Some(ch) = ch {
                    let data = if let Some(short) = escape_replacement(ch) {
                        format!("\\{short}")
                    } else {
                        let code = ch as u32;
                        if code <= 0xff {
                            format!("\\x{code:02X}")
                        } else if code <= 0xffff {
                            format!("\\u{code:04X}")
                        } else {
                            format!("\\U{code:08X}")
                        }
                    };
                    self.write(&data);
                    start = end + 1;
                }
            }
            if end > 0
                && end + 1 < chars.len()
                && (ch == Some(' ') || start >= end)
                // Upstream computes `column + (end - start)`, which goes
                // negative right after an escape. Compare across the addition
                // instead of subtracting into an unsigned type.
                && self.column.saturating_add(end) > BEST_WIDTH.saturating_add(start)
                && split
            {
                let data = format!("{}\\", slice(&chars, start, end));
                if start < end {
                    start = end;
                }
                self.write(&data);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if chars.get(start) == Some(&' ') {
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }
}

fn slice(chars: &[char], start: usize, end: usize) -> String {
    slice_chars(chars, start, end).iter().collect()
}

fn slice_chars(chars: &[char], start: usize, end: usize) -> &[char] {
    chars.get(start..end).unwrap_or_default()
}

/// `check_simple_key`. A key that is empty, multiline or long enough to risk
/// the 1024-character simple-key limit gets the explicit `? key` form instead.
fn is_simple_key(key: &str) -> bool {
    let analysis = analyze_scalar(key);
    key.chars().count() < 128 && !analysis.empty && !analysis.multiline
}

/// `choose_scalar_style`, minus the flow and block-literal branches: upstream
/// always dumps with `default_flow_style=False` and never sets an explicit style.
fn choose_style(analysis: &Analysis, implicit_plain: bool, simple_key: bool) -> Style {
    if implicit_plain
        && !(simple_key && (analysis.empty || analysis.multiline))
        && analysis.allow_block_plain
    {
        return Style::Plain;
    }
    if analysis.allow_single_quoted && !(simple_key && analysis.multiline) {
        return Style::Single;
    }
    Style::Double
}

#[derive(Debug)]
// Mirrors upstream's `ScalarAnalysis`; grouping these would hide the mapping.
#[allow(clippy::struct_excessive_bools)]
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

/// `analyze_scalar`, keeping only the outcomes a block dump can reach.
///
/// `allow_unicode` is false in upstream's configuration, so any character
/// outside printable ASCII counts as special and forces double quotes.
// Kept as one pass so it reads against the Python side by side.
#[allow(clippy::too_many_lines)]
fn analyze_scalar(scalar: &str) -> Analysis {
    if scalar.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_block_plain: true,
            allow_single_quoted: true,
        };
    }

    let chars: Vec<char> = scalar.chars().collect();
    let len = chars.len();
    let mut block_indicators = false;
    let mut line_breaks = false;
    let mut special_characters = false;
    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;

    if scalar.starts_with("---") || scalar.starts_with("...") {
        block_indicators = true;
    }

    let is_space_like =
        |ch: char| matches!(ch, '\0' | ' ' | '\t' | '\r' | '\n') || is_break(ch);

    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace =
        len == 1 || chars.get(1).copied().is_some_and(is_space_like);
    let mut previous_space = false;
    let mut previous_break = false;

    for index in 0..len {
        let Some(&ch) = chars.get(index) else { break };

        if index == 0 {
            if matches!(ch, '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!')
                || matches!(ch, '|' | '>' | '\'' | '"' | '%' | '@' | '`')
            {
                block_indicators = true;
            }
            if matches!(ch, '?' | ':') && followed_by_whitespace {
                block_indicators = true;
            }
            if ch == '-' && followed_by_whitespace {
                block_indicators = true;
            }
        } else {
            if ch == ':' && followed_by_whitespace {
                block_indicators = true;
            }
            if ch == '#' && preceded_by_whitespace {
                block_indicators = true;
            }
        }

        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7e').contains(&ch)) {
            special_characters = true;
        }

        if ch == ' ' {
            if index == 0 {
                leading_space = true;
            }
            if index == len - 1 {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            if index == 0 {
                leading_break = true;
            }
            if index == len - 1 {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }

        preceded_by_whitespace = is_space_like(ch);
        followed_by_whitespace = index + 2 >= len
            || chars.get(index + 2).copied().is_some_and(is_space_like);
    }

    let mut allow_block_plain = true;
    let mut allow_single_quoted = true;

    if leading_space || leading_break || trailing_space || trailing_break {
        allow_block_plain = false;
    }
    if break_space {
        allow_block_plain = false;
        allow_single_quoted = false;
    }
    if space_break || special_characters {
        allow_block_plain = false;
        allow_single_quoted = false;
    }
    if line_breaks {
        allow_block_plain = false;
    }
    if block_indicators {
        allow_block_plain = false;
    }

    Analysis {
        empty: false,
        multiline: line_breaks,
        allow_block_plain,
        allow_single_quoted,
    }
}

/// The scalar's text and whether it re-reads as itself unquoted.
fn scalar_text(value: &Value) -> (String, bool) {
    match value {
        Value::Null => ("null".to_owned(), true),
        Value::Bool(true) => ("true".to_owned(), true),
        Value::Bool(false) => ("false".to_owned(), true),
        Value::Number(number) => (format_number(number), true),
        Value::String(text) => (text.clone(), plain_resolves_to_str(text)),
        // Collections never reach here.
        other => (other.to_string(), true),
    }
}

/// `represent_float` / `represent_int`. Python's `repr` for a float always
/// carries a `.` or an exponent, so a bare integer-valued float gains `.0`.
fn format_number(number: &serde_json::Number) -> String {
    if let Some(int) = number.as_i64() {
        return int.to_string();
    }
    if let Some(int) = number.as_u64() {
        return int.to_string();
    }
    let Some(float) = number.as_f64() else {
        return number.to_string();
    };
    if float.is_nan() {
        return ".nan".to_owned();
    }
    if float.is_infinite() {
        return if float > 0.0 { ".inf" } else { "-.inf" }.to_owned();
    }
    let text = format!("{float}");
    if text.contains('.') {
        return text;
    }
    match text.split_once('e') {
        Some((mantissa, exponent)) => format!("{mantissa}.0e{exponent}"),
        None => format!("{text}.0"),
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

    #[test]
    fn a_flat_mapping_matches_upstream() {
        let value = json!({"status": "done", "datasource": "azure"});
        assert_eq!(dumps(&value), "---\ndatasource: azure\nstatus: done\n...\n");
    }

    #[test]
    fn empty_collections_stay_in_flow_style() {
        let value = json!({"errors": [], "recoverable_errors": {}});
        assert_eq!(
            dumps(&value),
            "---\nerrors: []\nrecoverable_errors: {}\n...\n"
        );
    }

    #[test]
    fn a_nested_mapping_indents_by_four() {
        let value = json!({"a": {"b": {"c": 1}}});
        assert_eq!(dumps(&value), "---\na:\n    b:\n        c: 1\n...\n");
    }

    #[test]
    fn a_sequence_under_a_key_stays_at_the_parent_indent() {
        let value = json!({"outer": {"errors": ["one", "two"]}});
        assert_eq!(
            dumps(&value),
            "---\nouter:\n    errors:\n    - one\n    - two\n...\n"
        );
    }

    #[test]
    fn a_nested_sequence_is_indented() {
        let value = json!([["a"]]);
        assert_eq!(dumps(&value), "---\n-   - a\n...\n");
    }

    #[test]
    fn a_long_scalar_folds_at_the_width_budget() {
        let detail = "DataSourceAzure [seed=/dev/sr0] failed to identify the instance \
                      because the metadata service did not respond";
        let value = json!({ "detail": detail });
        assert_eq!(
            dumps(&value),
            "---\ndetail: DataSourceAzure [seed=/dev/sr0] failed to identify the instance \
             because the\n    metadata service did not respond\n...\n"
        );
    }

    #[test]
    fn a_string_that_would_reload_as_another_type_is_quoted() {
        let value = json!({"a": "yes", "b": "0600", "c": "", "d": "null", "e": "1.5"});
        assert_eq!(
            dumps(&value),
            "---\na: 'yes'\nb: '0600'\nc: ''\nd: 'null'\ne: '1.5'\n...\n"
        );
    }

    #[test]
    fn booleans_numbers_and_null_stay_plain() {
        let value = json!({"a": true, "b": 3, "c": null, "d": 1.5});
        assert_eq!(dumps(&value), "---\na: true\nb: 3\nc: null\nd: 1.5\n...\n");
    }

    #[test]
    fn an_integer_valued_float_keeps_its_point() {
        let value = json!({ "t": 1_756_757_172.0 });
        assert_eq!(dumps(&value), "---\nt: 1756757172.0\n...\n");
    }

    #[test]
    fn non_ascii_forces_double_quotes_because_allow_unicode_is_false() {
        let value = json!({ "a": "caf\u{e9}" });
        assert_eq!(dumps(&value), "---\na: \"caf\\xE9\"\n...\n");
    }

    #[test]
    fn a_newline_is_single_quoted_and_doubled() {
        let value = json!({ "a": "one\ntwo" });
        assert_eq!(dumps(&value), "---\na: 'one\n\n    two'\n...\n");
    }

    #[test]
    fn a_leading_indicator_forces_quoting() {
        let value = json!({"a": "- dash", "b": "#hash", "c": "key: value"});
        assert_eq!(
            dumps(&value),
            "---\na: '- dash'\nb: '#hash'\nc: 'key: value'\n...\n"
        );
    }
}
