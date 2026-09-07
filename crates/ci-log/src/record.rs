//! Log levels and record formatting, matching Python's `logging` closely enough
//! that `cloud-init analyze` can parse what this writes.

use std::fmt;

/// Python's numeric levels plus the two cloud-init adds in
/// `loggers.define_extra_loggers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warning,
    Deprecated,
    Error,
    Critical,
}

impl Level {
    #[must_use]
    pub fn number(self) -> u32 {
        match self {
            Self::Trace => 5,
            Self::Debug => 10,
            Self::Info => 20,
            Self::Warning => 30,
            Self::Deprecated => 35,
            Self::Error => 40,
            Self::Critical => 50,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Deprecated => "DEPRECATED",
            Self::Error => "ERROR",
            Self::Critical => "CRITICAL",
        }
    }

    /// The spellings `logging.getLevelName` accepts, plus cloud-init's two.
    /// `WARN` and `FATAL` are Python's own aliases.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARNING" | "WARN" => Some(Self::Warning),
            "DEPRECATED" => Some(Self::Deprecated),
            "ERROR" => Some(Self::Error),
            "CRITICAL" | "FATAL" => Some(Self::Critical),
            _ => None,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One log call. `source` stands in for Python's `%(filename)s`: the log is a
/// compatibility surface, so it carries the upstream module name that would
/// have emitted the same line.
#[derive(Debug)]
pub struct Record<'a> {
    pub level: Level,
    pub source: &'a str,
    pub message: &'a str,
    pub epoch: f64,
}

/// The `%(...)s` fields cloud-init's shipped formatters use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Literal(String),
    Asctime,
    Filename,
    Levelname,
    Message,
}

/// A parsed `logging.Formatter` format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatter {
    pieces: Vec<Piece>,
}

impl Default for Formatter {
    fn default() -> Self {
        Self::parse("%(asctime)s - %(filename)s[%(levelname)s]: %(message)s")
            .unwrap_or(Self { pieces: Vec::new() })
    }
}

impl Formatter {
    /// `None` for a format naming a field the port does not carry, which is how
    /// an unsupported logging config gets rejected rather than half-applied.
    #[must_use]
    pub fn parse(format: &str) -> Option<Self> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut rest = format;
        while let Some(start) = rest.find("%(") {
            literal.push_str(rest.get(..start)?);
            let after = rest.get(start + 2..)?;
            let end = after.find(')')?;
            let name = after.get(..end)?;
            // Only the `s` conversion appears in any shipped format.
            let tail = after.get(end + 1..)?;
            let tail = tail.strip_prefix('s')?;
            let field = match name {
                "asctime" => Piece::Asctime,
                "filename" => Piece::Filename,
                "levelname" => Piece::Levelname,
                "message" => Piece::Message,
                _ => return None,
            };
            if !literal.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut literal)));
            }
            pieces.push(field);
            rest = tail;
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }
        Some(Self { pieces })
    }

    #[must_use]
    pub fn render(&self, record: &Record<'_>) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => out.push_str(text),
                Piece::Asctime => {
                    out.push_str(&ci_core::time::format_log_stamp(record.epoch));
                }
                Piece::Filename => out.push_str(record.source),
                Piece::Levelname => out.push_str(record.level.name()),
                Piece::Message => out.push_str(record.message),
            }
        }
        out
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

    fn record<'a>(level: Level, source: &'a str, message: &'a str) -> Record<'a> {
        Record {
            level,
            source,
            message,
            epoch: 1_700_000_000.5,
        }
    }

    #[test]
    fn the_default_format_is_the_one_analyze_parses() {
        let line = Formatter::default().render(&record(
            Level::Warning,
            "stages.py",
            "no datasource",
        ));

        assert_eq!(
            line,
            "2023-11-14 22:13:20,500 - stages.py[WARNING]: no datasource"
        );
    }

    #[test]
    fn the_syslog_format_keeps_its_leading_tag() {
        let formatter =
            Formatter::parse("[CLOUDINIT] %(filename)s[%(levelname)s]: %(message)s")
                .unwrap();

        assert_eq!(
            formatter.render(&record(Level::Debug, "util.py", "hi")),
            "[CLOUDINIT] util.py[DEBUG]: hi"
        );
    }

    #[test]
    fn a_format_naming_an_unsupported_field_is_rejected() {
        assert!(Formatter::parse("%(process)d %(message)s").is_none());
        assert!(Formatter::parse("%(thread)s: %(message)s").is_none());
        assert!(Formatter::parse("%(message)s").is_some());
    }

    #[test]
    fn the_two_extra_levels_sort_where_upstream_puts_them() {
        assert!(Level::Warning < Level::Deprecated);
        assert!(Level::Deprecated < Level::Error);
        assert!(Level::Trace < Level::Debug);
        assert_eq!(Level::Deprecated.number(), 35);
        assert_eq!(Level::Trace.number(), 5);
    }

    #[test]
    fn warn_and_fatal_are_accepted_as_aliases() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warning));
        assert_eq!(Level::parse("FATAL"), Some(Level::Critical));
        assert_eq!(Level::parse("nosuch"), None);
    }
}
