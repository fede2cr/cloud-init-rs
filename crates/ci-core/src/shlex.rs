//! `shlex.split` in POSIX mode, and the one `util` helper built on it.
//!
//! Upstream reaches for it twice on the initramfs path — once to tokenise the
//! kernel command line, once to read klibc's shell-syntax config files — and
//! the two differ only in whether `#` starts a comment.

use std::collections::BTreeMap;
use std::fmt;

/// Python's `shlex.whitespace`.
const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

/// The two ways `shlex.split` fails, carrying the text Python raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoClosingQuotation,
    NoEscapedCharacter,
    /// `load_shell_content`'s own failure: `key, value = line.split("=", 1)`
    /// on a token with no `=`.
    NotEnoughValuesToUnpack,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoClosingQuotation => "No closing quotation",
            Self::NoEscapedCharacter => "No escaped character",
            Self::NotEnoughValuesToUnpack => {
                "not enough values to unpack (expected 2, got 1)"
            }
        })
    }
}

impl std::error::Error for Error {}

enum State {
    Whitespace,
    Word,
    Single,
    Double,
}

/// `shlex.split(text, comments=comments)`, which is POSIX mode with
/// `whitespace_split` on.
///
/// # Errors
///
/// An unterminated quote or a trailing backslash, both of which are
/// `ValueError` upstream.
pub fn split(text: &str, comments: bool) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let mut token = String::new();
    // Python emits an empty token when it came from `''` or `""`, and drops it
    // otherwise. Reset per token, not per call.
    let mut quoted = false;
    let mut state = State::Whitespace;
    let mut chars = text.chars();

    macro_rules! escape {
        ($quote:expr) => {{
            let next = chars.next().ok_or(Error::NoEscapedCharacter)?;
            // Inside double quotes a backslash only escapes the quote or
            // itself; anything else keeps both characters.
            if let Some(quote) = $quote {
                if next != '\\' && next != quote {
                    token.push('\\');
                }
            }
            token.push(next);
        }};
    }

    while let Some(c) = chars.next() {
        match state {
            State::Whitespace => {
                if WHITESPACE.contains(&c) {
                } else if comments && c == '#' {
                    skip_line(&mut chars);
                } else {
                    match c {
                        '\'' => {
                            quoted = true;
                            state = State::Single;
                        }
                        '"' => {
                            quoted = true;
                            state = State::Double;
                        }
                        '\\' => {
                            escape!(None);
                            state = State::Word;
                        }
                        _ => {
                            token.push(c);
                            state = State::Word;
                        }
                    }
                }
            }
            State::Word => {
                let end = if WHITESPACE.contains(&c) {
                    true
                } else if comments && c == '#' {
                    skip_line(&mut chars);
                    true
                } else {
                    match c {
                        '\'' => {
                            quoted = true;
                            state = State::Single;
                        }
                        '"' => {
                            quoted = true;
                            state = State::Double;
                        }
                        '\\' => escape!(None),
                        _ => token.push(c),
                    }
                    false
                };
                if end {
                    // A word state always has something to emit: it was
                    // entered by a character, or left by a closing quote.
                    out.push(std::mem::take(&mut token));
                    quoted = false;
                    state = State::Whitespace;
                }
            }
            State::Single => {
                if c == '\'' {
                    state = State::Word;
                } else {
                    token.push(c);
                }
            }
            State::Double => {
                if c == '"' {
                    state = State::Word;
                } else if c == '\\' {
                    escape!(Some('"'));
                } else {
                    token.push(c);
                }
            }
        }
    }

    match state {
        State::Single | State::Double => return Err(Error::NoClosingQuotation),
        State::Word => out.push(token),
        State::Whitespace => {
            if quoted {
                out.push(token);
            }
        }
    }
    Ok(out)
}

fn skip_line(chars: &mut std::str::Chars<'_>) {
    for next in chars.by_ref() {
        if next == '\n' {
            break;
        }
    }
}

/// `util.load_shell_content` with its default arguments.
///
/// Comments are on, an empty value drops the key rather than storing it, and
/// the last occurrence of a key wins.
///
/// # Errors
///
/// A token with no `=` in it, which upstream lets escape as the `ValueError`
/// from tuple unpacking.
pub fn load_shell_content(content: &str) -> Result<BTreeMap<String, String>, Error> {
    let mut data = BTreeMap::new();
    for token in split(content, true)? {
        let (key, value) = token
            .split_once('=')
            .ok_or(Error::NotEnoughValuesToUnpack)?;
        if !value.is_empty() {
            data.insert(key.to_owned(), value.to_owned());
        }
    }
    Ok(data)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(text: &str) -> Vec<String> {
        split(text, true).unwrap()
    }

    #[test]
    fn runs_of_whitespace_collapse() {
        assert_eq!(s("a b  c"), ["a", "b", "c"]);
        assert_eq!(s("a\nb"), ["a", "b"]);
        assert!(s("   ").is_empty());
        assert!(s("").is_empty());
    }

    #[test]
    fn both_quote_styles_hold_a_token_together() {
        assert_eq!(s("\"a b\" c"), ["a b", "c"]);
        assert_eq!(s("'a b' c"), ["a b", "c"]);
    }

    /// Quotes are removed mid-word too, so the pieces run together.
    #[test]
    fn quotes_concatenate_rather_than_split() {
        assert_eq!(s("a\"b\"c"), ["abc"]);
    }

    /// An empty token survives only when quotes put it there.
    #[test]
    fn an_empty_quoted_token_is_still_a_token() {
        assert_eq!(s("\"\""), [""]);
        assert_eq!(s("K=\"\""), ["K="]);
    }

    #[test]
    fn a_comment_runs_to_the_end_of_its_line_only_when_asked() {
        assert_eq!(s("a #b c"), ["a"]);
        assert_eq!(s("#whole line"), Vec::<String>::new());
        assert_eq!(s("K=v # trailing\nJ=w"), ["K=v", "J=w"]);
        assert_eq!(split("a #b c", false).unwrap(), ["a", "#b", "c"]);
    }

    /// And it does not need whitespace in front of it.
    #[test]
    fn a_comment_can_start_inside_a_word() {
        assert_eq!(s("a#b c"), ["a"]);
        assert_eq!(split("a#b c", false).unwrap(), ["a#b", "c"]);
    }

    #[test]
    fn a_bare_backslash_escapes_anything() {
        assert_eq!(s("a\\ b"), ["a b"]);
        assert_eq!(s("a\\\\b"), ["a\\b"]);
    }

    /// Inside double quotes only the quote and the backslash are escapable;
    /// every other pair keeps both characters.
    #[test]
    fn a_backslash_in_double_quotes_escapes_almost_nothing() {
        assert_eq!(s("\"a\\\"b\""), ["a\"b"]);
        assert_eq!(s("\"a\\\\\""), ["a\\"]);
        assert_eq!(s("\"a\\nb\""), ["a\\nb"]);
        assert_eq!(s("\"a$b`c\\d\""), ["a$b`c\\d"]);
        assert_eq!(s("\"a\\\nb\""), ["a\\\nb"]);
    }

    /// Single quotes have no escapes at all, so the backslash is literal.
    #[test]
    fn single_quotes_are_opaque() {
        assert_eq!(s("'a\\'"), ["a\\"]);
    }

    #[test]
    fn an_unclosed_quote_or_escape_is_an_error() {
        assert_eq!(split("\"abc", true), Err(Error::NoClosingQuotation));
        assert_eq!(split("'abc", true), Err(Error::NoClosingQuotation));
        assert_eq!(split("'''", true), Err(Error::NoClosingQuotation));
        assert_eq!(split("abc\\", true), Err(Error::NoEscapedCharacter));
    }

    #[test]
    fn shell_content_drops_empty_values_and_comments() {
        let data = load_shell_content("K=v\nJ=\nL=1").unwrap();
        assert_eq!(data.get("K").map(String::as_str), Some("v"));
        assert_eq!(data.get("J"), None);
        assert_eq!(data.get("L").map(String::as_str), Some("1"));
        assert_eq!(
            load_shell_content("DEVICE=eth0\n#c\nPROTO=dhcp\n")
                .unwrap()
                .len(),
            2
        );
    }

    /// The split is on whitespace, not on lines, so a quoted value may hold
    /// spaces and two assignments may share a line.
    #[test]
    fn shell_content_is_tokenised_not_line_split() {
        assert_eq!(
            load_shell_content("DOMAINSEARCH='a.com b.com'\n")
                .unwrap()
                .get("DOMAINSEARCH")
                .map(String::as_str),
            Some("a.com b.com")
        );
        assert_eq!(load_shell_content("K=v J=w").unwrap().len(), 2);
    }

    #[test]
    fn the_last_assignment_to_a_key_wins() {
        assert_eq!(
            load_shell_content("K=v\nK=w\n")
                .unwrap()
                .get("K")
                .map(String::as_str),
            Some("w")
        );
    }

    #[test]
    fn a_token_without_an_equals_is_an_error() {
        assert_eq!(
            load_shell_content("novalue\n"),
            Err(Error::NotEnoughValuesToUnpack)
        );
    }
}
