//! `cloudinit/log/loggers.py`: the root logger cloud-init configures from
//! `log_cfgs`, and the warning collector that feeds `status.json`.

pub mod fileconfig;
pub mod record;

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use ci_config::Object;
use serde_json::Value;

use fileconfig::{Config, SinkSpec, Target};
pub use record::{Formatter, Level, Record};

#[derive(Debug)]
enum Writer {
    Stderr,
    Stdout,
    File(std::fs::File),
}

#[derive(Debug)]
struct Sink {
    writer: Writer,
    level: Level,
    formatter: Formatter,
}

impl Sink {
    fn open(spec: &SinkSpec) -> std::io::Result<Self> {
        let writer = match &spec.target {
            Target::Stderr => Writer::Stderr,
            Target::Stdout => Writer::Stdout,
            // `FileHandler` opens eagerly, so a missing directory is what makes
            // upstream fall through to the next `log_cfgs` entry.
            Target::File(path) => {
                Writer::File(OpenOptions::new().append(true).create(true).open(path)?)
            }
        };
        Ok(Self {
            writer,
            level: spec.level,
            formatter: spec.formatter.clone(),
        })
    }

    fn write(&mut self, line: &str) {
        let _ = match &mut self.writer {
            Writer::Stderr => writeln!(std::io::stderr(), "{line}"),
            Writer::Stdout => writeln!(std::io::stdout(), "{line}"),
            Writer::File(file) => writeln!(file, "{line}"),
        };
    }

    fn flush(&mut self) {
        let _ = match &mut self.writer {
            Writer::Stderr => std::io::stderr().flush(),
            Writer::Stdout => std::io::stdout().flush(),
            Writer::File(file) => file.flush(),
        };
    }
}

/// The root logger. Upstream keeps this in `logging`'s module-level registry;
/// here it is a value, so tests running in parallel cannot see each other's log.
#[derive(Debug)]
pub struct Logger {
    sinks: Vec<Sink>,
    root_level: Level,
    recoverable: BTreeMap<String, Vec<String>>,
    /// Set by [`Logger::capturing`]: every record, formatted the way a
    /// differential harness compares them.
    captured: Option<Vec<String>>,
}

impl Default for Logger {
    /// `setup_basic_logging`: stderr at `DEBUG` with the default format.
    fn default() -> Self {
        Self::basic(Level::Debug)
    }
}

impl Logger {
    #[must_use]
    pub fn silent() -> Self {
        Self {
            sinks: Vec::new(),
            root_level: Level::Debug,
            recoverable: BTreeMap::new(),
            captured: None,
        }
    }

    /// A silent logger that keeps every record as
    /// `"<source>[<LEVEL>]: <message>"` -- upstream's own format minus the
    /// timestamp, which nothing can compare.
    #[must_use]
    pub fn capturing() -> Self {
        Self {
            captured: Some(Vec::new()),
            ..Self::silent()
        }
    }

    /// What [`Logger::capturing`] has kept, in order.
    #[must_use]
    pub fn captured(&self) -> &[String] {
        self.captured.as_deref().unwrap_or_default()
    }

    #[must_use]
    pub fn basic(level: Level) -> Self {
        Self {
            sinks: vec![Sink {
                writer: Writer::Stderr,
                level,
                formatter: Formatter::default(),
            }],
            root_level: level,
            recoverable: BTreeMap::new(),
            captured: None,
        }
    }

    /// `setup_logging`: the first `log_cfgs` entry that applies wins, and a
    /// config naming a file under a directory that does not exist yet is
    /// skipped rather than fatal.
    #[must_use]
    pub fn from_config(cfg: &Object) -> Self {
        let mut logger = Self::silent();
        logger.reconfigure(cfg);
        logger
    }

    /// Swaps the sinks for the ones `cfg` asks for, keeping the recoverable
    /// errors already collected. Upstream keeps them by sharing one class-level
    /// dict between every exporter it ever attaches, which is what makes it
    /// count some of them twice (COMPAT.md B25).
    pub fn reconfigure(&mut self, cfg: &Object) {
        let entries = log_cfgs(cfg);
        let tried = entries.len();
        for entry in entries {
            match load(&entry) {
                Ok((sinks, root_level)) => {
                    self.sinks = sinks;
                    self.root_level = root_level;
                    return;
                }
                Err(Some(why)) => {
                    // Upstream lets this out of `setup_logging` and the stage
                    // dies with it (COMPAT.md B27).
                    eprintln!("Ignoring logging config: {why}");
                }
                Err(None) => {}
            }
        }
        eprintln!("WARN: no logging configured! (tried {tried} configs)");
        if truthy(cfg.get("log_basic"), true) {
            eprintln!("Setting up basic logging...");
            self.sinks = vec![Sink {
                writer: Writer::Stderr,
                level: Level::Debug,
                formatter: Formatter::default(),
            }];
            self.root_level = Level::Debug;
        } else {
            self.sinks = Vec::new();
        }
    }

    /// The level a record must reach to be seen by any sink at all.
    #[must_use]
    pub fn root_level(&self) -> Level {
        self.root_level
    }

    pub fn log(&mut self, level: Level, source: &str, message: &str) {
        if level < self.root_level {
            return;
        }
        if level >= Level::Warning {
            self.recoverable
                .entry(level.name().to_owned())
                .or_default()
                .push(message.to_owned());
        }
        if let Some(captured) = self.captured.as_mut() {
            captured.push(format!("{source}[{}]: {message}", level.name()));
        }
        let record = Record {
            level,
            source,
            message,
            epoch: ci_core::time::now_epoch(),
        };
        for sink in &mut self.sinks {
            if level >= sink.level {
                let line = sink.formatter.render(&record);
                sink.write(&line);
            }
        }
    }

    pub fn debug(&mut self, source: &str, message: &str) {
        self.log(Level::Debug, source, message);
    }

    pub fn info(&mut self, source: &str, message: &str) {
        self.log(Level::Info, source, message);
    }

    pub fn warning(&mut self, source: &str, message: &str) {
        self.log(Level::Warning, source, message);
    }

    pub fn error(&mut self, source: &str, message: &str) {
        self.log(Level::Error, source, message);
    }

    /// `LogExporter.export_logs`, as `status.json` wants it. Upstream counts a
    /// warning once per attached exporter and then puts the result through a
    /// `set`; this keeps one entry per call, in the order they were logged
    /// (COMPAT.md B25 and B26).
    #[must_use]
    pub fn recoverable_errors(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (level, messages) in &self.recoverable {
            let messages = messages.iter().map(|m| Value::String(m.clone())).collect();
            out.insert(level.clone(), Value::Array(messages));
        }
        Value::Object(out)
    }

    pub fn clear_recoverable_errors(&mut self) {
        self.recoverable.clear();
    }

    pub fn flush(&mut self) {
        for sink in &mut self.sinks {
            sink.flush();
        }
    }
}

/// `Err(None)` for a config that simply did not apply, which upstream reaches
/// through `suppress(FileNotFoundError)` and says nothing about.
fn load(entry: &str) -> Result<(Vec<Sink>, Level), Option<String>> {
    let Some(text) = config_text(entry) else {
        return Err(None);
    };
    let Config { root_level, sinks } =
        fileconfig::parse(&text).map_err(|e| Some(e.to_string()))?;
    let mut opened = Vec::new();
    for spec in &sinks {
        match Sink::open(spec) {
            Ok(sink) => opened.push(sink),
            Err(_) => return Err(None),
        }
    }
    Ok((opened, root_level))
}

/// A `log_cfgs` entry is either a path to a config or the config itself.
fn config_text(entry: &str) -> Option<String> {
    if entry.starts_with('/') && Path::new(entry).is_file() {
        return std::fs::read_to_string(entry).ok();
    }
    Some(entry.to_owned())
}

/// `logcfg` is the old key and wins outright; list entries are joined with
/// newlines so an array of INI fragments becomes one config.
fn log_cfgs(cfg: &Object) -> Vec<String> {
    if let Some(Value::String(one)) = cfg.get("logcfg") {
        if !one.is_empty() {
            return vec![one.clone()];
        }
    }
    let Some(Value::Array(entries)) = cfg.get("log_cfgs") else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| match entry {
            Value::String(text) => text.clone(),
            Value::Array(parts) => {
                parts.iter().map(scalar_text).collect::<Vec<_>>().join("\n")
            }
            other => scalar_text(other),
        })
        .collect()
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn truthy(value: Option<&Value>, default: bool) -> bool {
    match value {
        None => default,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Null) => false,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
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

    const BASE: &str = "\
[loggers]
keys=root

[handlers]
keys=cloudLogHandler

[formatters]
keys=arg0Formatter

[logger_root]
level=DEBUG
handlers=cloudLogHandler

[formatter_arg0Formatter]
format=%(asctime)s - %(filename)s[%(levelname)s]: %(message)s
";

    fn file_handler(path: &Path) -> String {
        format!(
            "{BASE}[handler_cloudLogHandler]\nclass=FileHandler\nlevel=DEBUG\n\
             formatter=arg0Formatter\nargs=('{}', 'a')\n",
            path.display()
        )
    }

    fn config(entries: Vec<Value>) -> Object {
        let mut cfg = Object::new();
        cfg.insert("log_cfgs".to_owned(), Value::Array(entries));
        cfg
    }

    #[test]
    fn a_file_config_writes_lines_analyze_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let cfg = config(vec![Value::String(file_handler(&log))]);

        let mut logger = Logger::from_config(&cfg);
        logger.warning("stages.py", "no datasource");
        logger.flush();

        let written = std::fs::read_to_string(&log).unwrap();
        assert!(
            written.ends_with(" - stages.py[WARNING]: no datasource\n"),
            "{written:?}"
        );
    }

    #[test]
    fn the_first_config_that_applies_wins() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.log");
        let second = dir.path().join("second.log");
        let cfg = config(vec![
            Value::String(file_handler(&first)),
            Value::String(file_handler(&second)),
        ]);

        let mut logger = Logger::from_config(&cfg);
        logger.debug("util.py", "hello");
        logger.flush();

        assert!(first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn a_config_naming_a_missing_directory_falls_through_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.log");
        let cfg = config(vec![
            Value::String(file_handler(Path::new("/no/such/dir/x.log"))),
            Value::String(file_handler(&good)),
        ]);

        let mut logger = Logger::from_config(&cfg);
        logger.debug("util.py", "hello");
        logger.flush();

        assert!(good.exists());
    }

    #[test]
    fn an_array_entry_is_joined_into_one_config() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let whole = file_handler(&log);
        let (head, tail) = whole.split_at(whole.find("[handler_").unwrap());
        let cfg = config(vec![Value::Array(vec![
            Value::String(head.to_owned()),
            Value::String(tail.to_owned()),
        ])]);

        let mut logger = Logger::from_config(&cfg);
        logger.debug("util.py", "hello");
        logger.flush();

        assert!(log.exists());
    }

    #[test]
    fn no_usable_config_falls_back_to_basic_logging() {
        let logger = Logger::from_config(&config(Vec::new()));

        assert_eq!(logger.sinks.len(), 1);
        assert_eq!(logger.root_level, Level::Debug);
    }

    #[test]
    fn reconfiguring_keeps_what_was_recorded_before_the_config_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let mut logger = Logger::silent();

        logger.warning("main.py", "before");
        logger.reconfigure(&config(vec![Value::String(file_handler(&log))]));
        logger.warning("stages.py", "after");
        logger.flush();

        assert_eq!(
            logger.recoverable_errors(),
            serde_json::json!({"WARNING": ["before", "after"]})
        );
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("after"), "{written:?}");
        assert!(!written.contains("before"), "{written:?}");
    }

    #[test]
    fn log_basic_false_leaves_nothing_configured() {
        let mut cfg = config(Vec::new());
        cfg.insert("log_basic".to_owned(), Value::Bool(false));

        assert!(Logger::from_config(&cfg).sinks.is_empty());
    }

    #[test]
    fn a_handler_below_the_root_level_never_sees_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let cfg = config(vec![Value::String(
            file_handler(&log)
                .replace("level=DEBUG\nhandlers", "level=ERROR\nhandlers"),
        )]);

        let mut logger = Logger::from_config(&cfg);
        logger.warning("stages.py", "swallowed");
        logger.flush();

        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
    }

    #[test]
    fn warnings_and_worse_are_collected_once_each_in_the_order_logged() {
        let mut logger = Logger::silent();

        logger.debug("util.py", "ignored");
        logger.warning("stages.py", "second");
        logger.warning("stages.py", "first");
        logger.warning("stages.py", "second");
        logger.error("main.py", "boom");

        assert_eq!(
            logger.recoverable_errors(),
            serde_json::json!({
                "ERROR": ["boom"],
                "WARNING": ["second", "first", "second"],
            })
        );
    }

    #[test]
    fn the_old_logcfg_key_wins_over_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.log");
        let new = dir.path().join("new.log");
        let mut cfg = config(vec![Value::String(file_handler(&new))]);
        cfg.insert("logcfg".to_owned(), Value::String(file_handler(&old)));

        let mut logger = Logger::from_config(&cfg);
        logger.debug("util.py", "hello");
        logger.flush();

        assert!(old.exists());
        assert!(!new.exists());
    }

    #[test]
    fn a_config_file_path_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("cloud-init.log");
        let path = dir.path().join("logging.conf");
        std::fs::write(&path, file_handler(&log)).unwrap();
        let cfg = config(vec![Value::String(path.display().to_string())]);

        let mut logger = Logger::from_config(&cfg);
        logger.debug("util.py", "hello");
        logger.flush();

        assert!(log.exists());
    }
}
