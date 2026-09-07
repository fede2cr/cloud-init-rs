//! Python string operations whose Rust namesakes differ.
//!
//! Small, but worth a home of their own: each one here is a place where the
//! obvious `str` method quietly disagrees with `CPython`, and a boot that reads
//! tenant-supplied text through the wrong one splits it differently than
//! upstream would.

/// Python's `str.splitlines`, which breaks on ten characters where
/// [`str::lines`] breaks on one.
///
/// An include list separated by `\r` or `\x0b` is two URLs upstream, so it is
/// two here. `\r\n` counts once.
#[must_use]
pub fn split_lines(text: &str) -> Vec<&str> {
    const BREAKS: [char; 10] = [
        '\n', '\u{b}', '\u{c}', '\r', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}',
        '\u{2028}', '\u{2029}',
    ];
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !BREAKS.contains(&c) {
            continue;
        }
        lines.push(text.get(start..at).unwrap_or(""));
        start = at + c.len_utf8();
        if c == '\r' {
            if let Some(&(next, '\n')) = chars.peek() {
                chars.next();
                start = next + 1;
            }
        }
    }
    if start < text.len() {
        lines.push(text.get(start..).unwrap_or(""));
    }
    lines
}

/// Python's `str.isspace` for one character, which is [`char::is_whitespace`]
/// plus the four C0 separators.
#[must_use]
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.split(None, maxsplit)`: leading whitespace is dropped, runs of
/// whitespace count once, and once `maxsplit` splits have happened the
/// remainder is returned verbatim — trailing whitespace and all.
///
/// [`str::splitn`] cannot express this; it would emit empty fields for a run
/// and strip nothing.
#[must_use]
pub fn split_whitespace_n(text: &str, maxsplit: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(|c| !is_space(c)) {
        rest = rest.get(start..).unwrap_or("");
        if out.len() == maxsplit {
            out.push(rest);
            break;
        }
        let Some(end) = rest.find(is_space) else {
            out.push(rest);
            break;
        };
        out.push(rest.get(..end).unwrap_or(""));
        rest = rest.get(end..).unwrap_or("");
    }
    out
}

/// Python's `str.strip()`.
#[must_use]
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
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
    fn the_seven_breaks_rust_does_not_know_about_still_split() {
        for sep in [
            '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        ] {
            assert_eq!(split_lines(&format!("a{sep}b")), vec!["a", "b"], "{sep:?}");
        }
    }

    #[test]
    fn a_crlf_is_one_break_and_a_trailing_break_adds_no_empty_line() {
        assert_eq!(split_lines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(split_lines("a\n"), vec!["a"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
    }
}
