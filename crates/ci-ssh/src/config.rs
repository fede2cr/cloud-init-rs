//! `sshd_config` reading: the keyword/argument line format from `man
//! sshd_config`, and the two keywords `extract_authorized_keys` consults.
//!
//! Only the parse is here. Deciding *which* file to read, and rewriting it,
//! belong to the halves that touch the filesystem.

use std::collections::BTreeMap;

use ci_log::Logger;

const SOURCE: &str = "ssh_util.py";

/// One line of an `sshd_config`.
///
/// A comment or blank keeps `key` empty and round-trips through [`Self::render`]
/// unchanged. Keywords are case-insensitive when looked up but keep their
/// original case when written back, so rewriting a file does not restyle the
/// parts of it nobody asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshdConfigLine {
    pub line: String,
    raw_key: Option<String>,
    pub value: Option<String>,
}

impl SshdConfigLine {
    #[must_use]
    pub fn comment(line: &str) -> Self {
        Self {
            line: line.to_owned(),
            raw_key: None,
            value: None,
        }
    }

    #[must_use]
    pub fn new(line: &str, key: &str, value: &str) -> Self {
        Self {
            line: line.to_owned(),
            raw_key: Some(key.to_owned()),
            value: Some(value.to_owned()),
        }
    }

    /// The keyword, lowercased, or `None` for a comment or blank.
    #[must_use]
    pub fn key(&self) -> Option<String> {
        self.raw_key.as_ref().map(|k| k.to_lowercase())
    }

    /// The keyword as it was written.
    #[must_use]
    pub fn raw_key(&self) -> Option<&str> {
        self.raw_key.as_deref()
    }

    /// `SshdConfigLine.__str__`.
    #[must_use]
    pub fn render(&self) -> String {
        let Some(key) = &self.raw_key else {
            return self.line.clone();
        };
        match &self.value {
            Some(value) if !value.is_empty() => format!("{key} {value}"),
            _ => key.clone(),
        }
    }
}

/// `parse_ssh_config_lines`.
///
/// A line splits on whitespace first and on `=` only if that yielded a single
/// token, which is why `Port = 22` parses as the keyword `Port` with the value
/// `= 22`. That is sshd's own precedence and upstream's, so it is reproduced.
/// A line with neither separator is dropped entirely.
pub fn parse_config_lines(lines: &[&str], log: &mut Logger) -> Vec<SshdConfigLine> {
    let mut ret = Vec::new();
    for raw in lines {
        let line = ci_core::pystr::strip(raw);
        if line.is_empty() || line.starts_with('#') {
            ret.push(SshdConfigLine::comment(line));
            continue;
        }
        let toks = ci_core::pystr::split_whitespace_n(line, 1);
        let pair = match (toks.first(), toks.get(1)) {
            (Some(key), Some(val)) => Some(((*key).to_owned(), (*val).to_owned())),
            _ => line
                .split_once('=')
                .map(|(key, val)| (key.to_owned(), val.to_owned())),
        };
        let Some((key, val)) = pair else {
            log.log(
                ci_log::Level::Debug,
                SOURCE,
                &format!(
                    "sshd_config: option \"{line}\" has no key/value pair, skipping it"
                ),
            );
            continue;
        };
        ret.push(SshdConfigLine::new(line, &key, &val));
    }
    ret
}

/// `parse_ssh_config_map`: the last value wins for a repeated keyword.
///
/// That is a real divergence from sshd itself, which takes the *first* value
/// for most keywords. It is upstream's behaviour and it only ever feeds
/// `AuthorizedKeysFile` and `StrictModes`, so it is reproduced rather than
/// corrected — see `docs/COMPAT.md`.
///
/// A line like `=value` parses to a keyword of `""`, which upstream's
/// truthiness test drops. The line survives in the list; only the map skips it.
#[must_use]
pub fn config_map(lines: &[SshdConfigLine]) -> BTreeMap<String, String> {
    let mut ret = BTreeMap::new();
    for line in lines {
        let Some(key) = line.key().filter(|k| !k.is_empty()) else {
            continue;
        };
        ret.insert(key, line.value.clone().unwrap_or_default());
    }
    ret
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

    fn parse(text: &str) -> Vec<SshdConfigLine> {
        let lines = ci_core::pystr::split_lines(text);
        parse_config_lines(&lines, &mut Logger::silent())
    }

    #[test]
    fn keywords_are_case_insensitive_but_keep_their_case() {
        let lines = parse("PermitRootLogin no\n");
        assert_eq!(lines[0].key().as_deref(), Some("permitrootlogin"));
        assert_eq!(lines[0].raw_key(), Some("PermitRootLogin"));
        assert_eq!(lines[0].render(), "PermitRootLogin no");
    }

    #[test]
    fn comments_and_blanks_round_trip() {
        let lines = parse("# hello\n\n   \n");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.key().is_none()));
        assert_eq!(lines[0].render(), "# hello");
        assert_eq!(lines[1].render(), "");
    }

    #[test]
    fn an_equals_form_is_accepted_only_without_whitespace() {
        assert_eq!(parse("Port=22\n")[0].value.as_deref(), Some("22"));
        // Whitespace wins, so the `=` lands in the value.
        assert_eq!(parse("Port = 22\n")[0].value.as_deref(), Some("= 22"));
    }

    #[test]
    fn a_lone_word_is_dropped_entirely() {
        assert!(parse("Compression\n").is_empty());
    }

    #[test]
    fn the_last_value_wins() {
        let map = config_map(&parse("StrictModes yes\nStrictModes no\n"));
        assert_eq!(map.get("strictmodes").map(String::as_str), Some("no"));
    }
}
