//! The shell string operations the script relies on, reproduced exactly.
//!
//! These look trivial and are not. `trim` strips POSIX `[[:space:]]`, which
//! includes the vertical tab that `char::is_ascii_whitespace` omits; `read var
//! < file` processes backslash escapes before it strips whitespace; and glob
//! matching is `case`'s, not a regular expression.

/// POSIX `[[:space:]]` in the C locale.
#[must_use]
pub fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

/// `trim()`: strip leading and trailing `[[:space:]]`.
#[must_use]
pub fn trim(value: &str) -> &str {
    value.trim_matches(is_space)
}

/// `unquote()`: drop one layer of matching `'` or `"`.
///
/// The shell pattern is `"*"` or `'*'`, which needs both quotes present, so a
/// lone quote character is left alone.
#[must_use]
pub fn unquote(value: &str) -> &str {
    let mut chars = value.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        return value;
    };
    if first == last && (first == '"' || first == '\'') {
        chars.as_str()
    } else {
        value
    }
}

/// `read var < file`: one logical line, backslash-processed, then IFS-trimmed.
///
/// `read` without `-r` treats `\` as an escape: `\<newline>` continues the line
/// and `\x` yields `x`. Only then is leading and trailing IFS whitespace
/// removed. Nothing in a real `/proc/cmdline` or DMI field exercises this, but
/// a seed that a hostile platform controls could.
#[must_use]
pub fn read_line(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                None | Some('\n') => {}
                Some(next) => out.push(next),
            },
            '\n' => break,
            _ => out.push(c),
        }
    }
    // `read` splits on IFS, whose whitespace characters are space, tab and
    // newline only -- not the full `[[:space:]]` set that `trim()` uses.
    out.trim_matches([' ', '\t', '\n']).to_owned()
}

/// `case "$s" in $pat)`: POSIX shell pattern matching.
///
/// `*`, `?` and bracket expressions, with `\` escaping the next character.
/// There is no `/` special-casing: this is `case`, not pathname expansion.
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let subject: Vec<char> = text.chars().collect();
    match_from(&pat, &subject)
}

fn match_from(pat: &[char], txt: &[char]) -> bool {
    let Some((&first, rest)) = pat.split_first() else {
        return txt.is_empty();
    };
    match first {
        '*' => {
            // Match the shortest prefix that lets the remainder match.
            for skip in 0..=txt.len() {
                if match_from(rest, txt.get(skip..).unwrap_or_default()) {
                    return true;
                }
            }
            false
        }
        '?' => match txt.split_first() {
            Some((_, tail)) => match_from(rest, tail),
            None => false,
        },
        '[' => match_bracket(rest, txt),
        '\\' => match (rest.split_first(), txt.split_first()) {
            (Some((&escaped, pat_tail)), Some((&c, txt_tail))) if escaped == c => {
                match_from(pat_tail, txt_tail)
            }
            // A trailing backslash is a literal backslash.
            (None, Some((&'\\', txt_tail))) => match_from(rest, txt_tail),
            _ => false,
        },
        _ => match txt.split_first() {
            Some((&c, tail)) if c == first => match_from(rest, tail),
            _ => false,
        },
    }
}

/// Handles `[abc]`, `[!abc]`, `[a-z]` and `[[:alpha:]]`.
///
/// An unterminated `[` is a literal `[`, which is what makes `${tmp#[}` in
/// `get_single_line_flow_sequence` strip a bracket rather than nothing.
fn match_bracket(pat: &[char], txt: &[char]) -> bool {
    let Some((&c, txt_tail)) = txt.split_first() else {
        return false;
    };
    let mut i = 0;
    let negated = matches!(pat.first(), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first_item = true;
    loop {
        let Some(&item) = pat.get(i) else {
            // Unterminated: the '[' was literal.
            return match_from_literal_bracket(pat, txt);
        };
        if item == ']' && !first_item {
            break;
        }
        first_item = false;
        if item == '[' && matches!(pat.get(i + 1), Some(':')) {
            let Some(end) = find_class_end(pat, i + 2) else {
                return match_from_literal_bracket(pat, txt);
            };
            let name: String = pat.get(i + 2..end).unwrap_or_default().iter().collect();
            if in_class(&name, c) {
                matched = true;
            }
            i = end + 2;
            continue;
        }
        // A range, unless the '-' is the last item before ']'.
        if matches!(pat.get(i + 1), Some('-'))
            && !matches!(pat.get(i + 2), Some(']') | None)
        {
            if let Some(&hi) = pat.get(i + 2) {
                if item <= c && c <= hi {
                    matched = true;
                }
            }
            i += 3;
            continue;
        }
        if item == c {
            matched = true;
        }
        i += 1;
    }
    if matched == negated {
        return false;
    }
    match_from(pat.get(i + 1..).unwrap_or_default(), txt_tail)
}

fn match_from_literal_bracket(pat: &[char], txt: &[char]) -> bool {
    match txt.split_first() {
        Some((&'[', tail)) => match_from(pat, tail),
        _ => false,
    }
}

fn find_class_end(pat: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(&c) = pat.get(i) {
        if c == ':' && matches!(pat.get(i + 1), Some(']')) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn in_class(name: &str, c: char) -> bool {
    match name {
        "alpha" => c.is_ascii_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_ascii_alphanumeric(),
        "space" => is_space(c),
        "upper" => c.is_ascii_uppercase(),
        "lower" => c.is_ascii_lowercase(),
        "punct" => c.is_ascii_punctuation(),
        "xdigit" => c.is_ascii_hexdigit(),
        "cntrl" => c.is_ascii_control(),
        "print" => c.is_ascii_graphic() || c == ' ',
        "graph" => c.is_ascii_graphic(),
        "blank" => c == ' ' || c == '\t',
        _ => false,
    }
}

/// `set -- $value` under a single-character IFS: split, dropping empty fields.
///
/// The shell collapses runs of an IFS whitespace character and drops leading
/// and trailing ones; for a non-whitespace IFS such as `,` it does not, but
/// every caller here passes the result through `trim`, so dropping empty
/// fields is only ever visible for whitespace.
#[must_use]
pub fn split_ifs(value: &str, sep: char) -> Vec<&str> {
    value.split(sep).filter(|s| !s.is_empty()).collect()
}

/// `set -- $value` with the default IFS (space, tab, newline).
#[must_use]
pub fn split_words(value: &str) -> Vec<&str> {
    value
        .split([' ', '\t', '\n'])
        .filter(|s| !s.is_empty())
        .collect()
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
    fn trims_the_posix_space_class() {
        assert_eq!(trim(" \t\u{b}x\u{c}\r\n"), "x");
        assert_eq!(trim(""), "");
    }

    #[test]
    fn unquotes_only_matched_pairs() {
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("\"\""), "");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("'x\""), "'x\"");
    }

    #[test]
    fn reads_a_line_the_way_read_does() {
        assert_eq!(read_line("  hello world  \nsecond"), "hello world");
        assert_eq!(read_line("a\\\nb"), "ab");
        assert_eq!(read_line("a\\tb"), "atb");
    }

    #[test]
    fn matches_the_patterns_the_script_uses() {
        assert!(glob_match("CloudStack*", "CloudStack Foo"));
        assert!(!glob_match("CloudStack*", "cloudstack"));
        assert!(glob_match("*.brightbox.com", "x.brightbox.com"));
        assert!(glob_match("[Ee][Cc]2*", "ec2abc"));
        assert!(glob_match("*2[0-9a-fA-F][Ee][Cc]", "45E12AEC"));
        assert!(glob_match("[Rr][Hh][Ee][Vv]", "RHEV"));
        assert!(glob_match("warn,[0-9]*", "warn,5"));
        assert!(!glob_match("warn,[0-9]*", "warn,x"));
        assert!(glob_match("i?86", "i686"));
        assert!(glob_match("*\\ None\\ ", " a, None "));
        assert!(!glob_match("*\\ None\\ ", " anone "));
        assert!(glob_match("2???-??-??", "2015-10-15"));
    }

    #[test]
    fn an_unterminated_bracket_is_literal() {
        assert!(glob_match("[", "["));
        assert!(!glob_match("[", "x"));
    }
}
