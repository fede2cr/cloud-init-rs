//! The `logging.config.fileConfig` subset cloud-init's shipped `log_cfgs` use.
//!
//! Python evaluates the `class` and `args` values of every handler section with
//! `eval` (COMPAT.md B28). This reads them instead, which is why only the forms
//! that actually appear in a logging config are accepted.

use std::path::PathBuf;

use crate::record::{Formatter, Level};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Stderr,
    Stdout,
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkSpec {
    pub target: Target,
    pub level: Level,
    pub formatter: Formatter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub root_level: Level,
    pub sinks: Vec<SinkSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The text is not a logging config the port can read at all.
    Malformed(String),
    /// A well-formed config asking for something the port does not implement,
    /// such as a syslog handler. Upstream would apply it.
    Unsupported(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed logging config: {why}"),
            Self::Unsupported(what) => {
                write!(f, "unsupported logging config: {what}")
            }
        }
    }
}

/// `[section] key=value`, enough of `configparser` for a logging config:
/// `#`/`;` comments, `=` or `:` separators, and indented continuation lines.
#[derive(Debug, Default)]
struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl Ini {
    fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut ini = Self::default();
        for raw in text.lines() {
            let trimmed = raw.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with(';')
            {
                continue;
            }
            if let Some(name) =
                trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']'))
            {
                ini.sections.push((name.trim().to_owned(), Vec::new()));
                continue;
            }
            let Some(section) = ini.sections.last_mut() else {
                return Err(ConfigError::Malformed(
                    "file contains no section headers".to_owned(),
                ));
            };
            let indented = raw.starts_with(' ') || raw.starts_with('\t');
            if let Some((key, value)) = split_entry(trimmed) {
                section.1.push((key, value));
            } else if indented {
                if let Some(last) = section.1.last_mut() {
                    last.1.push('\n');
                    last.1.push_str(trimmed);
                }
            } else {
                return Err(ConfigError::Malformed(format!("stray line: {trimmed}")));
            }
        }
        Ok(ini)
    }

    fn section(&self, name: &str) -> Option<&[(String, String)]> {
        self.sections
            .iter()
            .find(|(section, _)| section == name)
            .map(|(_, entries)| entries.as_slice())
    }

    fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

fn split_entry(line: &str) -> Option<(String, String)> {
    let cut = line
        .find('=')
        .into_iter()
        .chain(line.find(':'))
        .min()
        .filter(|cut| *cut > 0)?;
    let key = line.get(..cut)?.trim().to_owned();
    let value = line.get(cut + 1..)?.trim().to_owned();
    Some((key, value))
}

/// Reads a `log_cfgs` entry into the sinks it asks for.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let ini = Ini::parse(text)?;
    for required in ["loggers", "handlers", "formatters"] {
        if ini.section(required).is_none() {
            return Err(ConfigError::Malformed(format!("no [{required}] section")));
        }
    }
    let root_level = match ini.get("logger_root", "level") {
        Some(text) => Level::parse(text)
            .ok_or_else(|| ConfigError::Malformed(format!("bad root level {text}")))?,
        None => Level::Warning,
    };
    let names = ini.get("logger_root", "handlers").unwrap_or_default();
    let mut sinks = Vec::new();
    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        sinks.push(handler(&ini, name)?);
    }
    Ok(Config { root_level, sinks })
}

fn handler(ini: &Ini, name: &str) -> Result<SinkSpec, ConfigError> {
    let section = format!("handler_{name}");
    if ini.section(&section).is_none() {
        return Err(ConfigError::Malformed(format!("no [{section}] section")));
    }
    let class = ini
        .get(&section, "class")
        .ok_or_else(|| ConfigError::Malformed(format!("[{section}] has no class")))?;
    let args = ini.get(&section, "args").unwrap_or("()");
    let target = match class {
        "StreamHandler" => stream_target(args)?,
        "FileHandler" => Target::File(PathBuf::from(file_target(args)?)),
        other => {
            return Err(ConfigError::Unsupported(format!("handler class {other}")));
        }
    };
    let level = match ini.get(&section, "level") {
        // A handler with no level passes everything, as NOTSET does upstream.
        None | Some("NOTSET") => Level::Trace,
        Some(text) => Level::parse(text)
            .ok_or_else(|| ConfigError::Malformed(format!("bad level {text}")))?,
    };
    let formatter = match ini.get(&section, "formatter").unwrap_or_default() {
        "" => Formatter::parse("%(message)s")
            .ok_or_else(|| ConfigError::Malformed("default format".to_owned()))?,
        name => {
            let format =
                ini.get(&format!("formatter_{name}"), "format")
                    .ok_or_else(|| {
                        ConfigError::Malformed(format!("no format for {name}"))
                    })?;
            Formatter::parse(format).ok_or_else(|| {
                ConfigError::Unsupported(format!("format string {format}"))
            })?
        }
    };
    Ok(SinkSpec {
        target,
        level,
        formatter,
    })
}

fn stream_target(args: &str) -> Result<Target, ConfigError> {
    match first_arg(args).as_deref() {
        Some("sys.stderr") | None => Ok(Target::Stderr),
        Some("sys.stdout") => Ok(Target::Stdout),
        Some(other) => Err(ConfigError::Unsupported(format!("stream {other}"))),
    }
}

fn file_target(args: &str) -> Result<String, ConfigError> {
    let first = first_arg(args)
        .ok_or_else(|| ConfigError::Malformed("file handler has no path".to_owned()))?;
    string_literal(&first)
        .ok_or_else(|| ConfigError::Unsupported(format!("file path {first}")))
}

/// The first element of a Python tuple literal, without evaluating it.
fn first_arg(args: &str) -> Option<String> {
    let inner = args.trim().strip_prefix('(')?.strip_suffix(')')?;
    let mut depth = 0usize;
    let mut quote = None;
    for (index, byte) in inner.bytes().enumerate() {
        match (quote, byte) {
            (Some(q), b) if b == q => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'(' | b'[' | b'{') => depth += 1,
            (None, b')' | b']' | b'}') => depth = depth.checked_sub(1)?,
            (None, b',') if depth == 0 => {
                return Some(inner.get(..index)?.trim().to_owned());
            }
            _ => {}
        }
    }
    let only = inner.trim();
    (!only.is_empty()).then(|| only.to_owned())
}

fn string_literal(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let quote = *bytes.first()?;
    if (quote != b'\'' && quote != b'"')
        || bytes.last() != Some(&quote)
        || bytes.len() < 2
    {
        return None;
    }
    let inner = text.get(1..text.len() - 1)?;
    // A path with an escape in it is not something the port will guess at.
    (!inner.contains('\\') && !inner.contains(quote as char)).then(|| inner.to_owned())
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

    const BASE: &str = "\
[loggers]
keys=root,cloudinit

[handlers]
keys=consoleHandler,cloudLogHandler

[formatters]
keys=simpleFormatter,arg0Formatter

[logger_root]
level=DEBUG
handlers=consoleHandler,cloudLogHandler

[logger_cloudinit]
level=DEBUG
qualname=cloudinit
handlers=
propagate=1

[handler_consoleHandler]
class=StreamHandler
level=WARNING
formatter=arg0Formatter
args=(sys.stderr,)

[formatter_arg0Formatter]
format=%(asctime)s - %(filename)s[%(levelname)s]: %(message)s

[formatter_simpleFormatter]
format=[CLOUDINIT] %(filename)s[%(levelname)s]: %(message)s
";

    const FILE_HANDLER: &str = "\
[handler_cloudLogHandler]
class=FileHandler
level=DEBUG
formatter=arg0Formatter
args=('/var/log/cloud-init.log', 'a', 'UTF-8')
";

    const SYSLOG_HANDLER: &str = "\
[handler_cloudLogHandler]
class=handlers.SysLogHandler
level=DEBUG
formatter=simpleFormatter
args=(\"/dev/log\", handlers.SysLogHandler.LOG_USER)
";

    #[test]
    fn the_shipped_file_config_resolves_to_a_console_and_a_file_sink() {
        let config = parse(&format!("{BASE}{FILE_HANDLER}")).unwrap();

        assert_eq!(config.root_level, Level::Debug);
        assert_eq!(config.sinks.len(), 2);
        assert_eq!(config.sinks[0].target, Target::Stderr);
        assert_eq!(config.sinks[0].level, Level::Warning);
        assert_eq!(
            config.sinks[1].target,
            Target::File(PathBuf::from("/var/log/cloud-init.log"))
        );
        assert_eq!(config.sinks[1].level, Level::Debug);
    }

    #[test]
    fn the_shipped_syslog_config_is_unsupported_rather_than_malformed() {
        let error = parse(&format!("{BASE}{SYSLOG_HANDLER}")).unwrap_err();

        assert_eq!(
            error,
            ConfigError::Unsupported("handler class handlers.SysLogHandler".to_owned())
        );
    }

    #[test]
    fn text_that_is_not_a_config_is_rejected() {
        assert!(matches!(
            parse("not an ini at all"),
            Err(ConfigError::Malformed(_))
        ));
        assert!(matches!(
            parse("[loggers]\nkeys=root\n"),
            Err(ConfigError::Malformed(_))
        ));
    }

    #[test]
    fn a_handler_argument_that_is_not_a_literal_is_refused_not_evaluated() {
        let hostile = format!(
            "{BASE}[handler_cloudLogHandler]\nclass=FileHandler\nlevel=DEBUG\n\
             formatter=arg0Formatter\nargs=(__import__('os').system('id'),)\n"
        );

        assert!(matches!(parse(&hostile), Err(ConfigError::Unsupported(_))));
    }

    #[test]
    fn the_first_tuple_element_is_read_without_evaluating_the_rest() {
        assert_eq!(first_arg("(sys.stderr,)").as_deref(), Some("sys.stderr"));
        assert_eq!(
            first_arg("('/a/b.log', 'a', 'UTF-8')").as_deref(),
            Some("'/a/b.log'")
        );
        assert_eq!(first_arg("()"), None);
    }
}
