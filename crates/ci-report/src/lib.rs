//! Event reporting for cloud-init-rs.
//!
//! Port of `cloudinit/reporting`. Parts of cloud-init announce what they are
//! doing as start/finish pairs; handlers decide where those announcements go.

pub mod events;
pub mod handlers;
pub mod kvp;

use std::path::Path;

use ci_config::Object;
use ci_log::{Level, Logger};
use handlers::{Handler, LogHandler, PrintHandler};
use serde_json::Value;

/// The registered handlers, in registration order.
///
/// Upstream keeps this in a module-level registry. Holding it in a value keeps
/// two stages in one process from sharing reporting state by accident.
#[derive(Debug)]
pub struct Reporter {
    handlers: Vec<(String, Box<dyn Handler>)>,
}

impl Default for Reporter {
    /// `DEFAULT_CONFIG`: one `log` handler named `logging`.
    fn default() -> Self {
        Self {
            handlers: vec![(
                "logging".to_owned(),
                Box::new(LogHandler::at(Level::Debug)),
            )],
        }
    }
}

impl Reporter {
    /// A reporter with nothing registered, for callers that only want the
    /// event bookkeeping.
    pub fn silent() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn register(&mut self, name: &str, handler: Box<dyn Handler>) {
        self.unregister(name);
        self.handlers.push((name.to_owned(), handler));
    }

    pub fn unregister(&mut self, name: &str) {
        self.handlers.retain(|(known, _)| known != name);
    }

    pub fn publish(&mut self, event: &events::Event, logger: &mut Logger) {
        for (_, handler) in &mut self.handlers {
            handler.publish(event, logger);
        }
    }

    pub fn flush(&mut self, logger: &mut Logger) {
        for (_, handler) in &mut self.handlers {
            handler.flush(logger);
        }
    }

    /// `get_kvp_handler`: the handler registered under `telemetry`, if it is a
    /// Hyper-V KVP one.
    pub fn kvp_handler(&mut self) -> Option<&mut kvp::HyperVKvpHandler> {
        self.handlers
            .iter_mut()
            .find(|(name, _)| name == "telemetry")
            .and_then(|(_, handler)| handler.as_kvp())
    }

    /// `update_configuration`: apply the `reporting` config key.
    ///
    /// A handler whose configuration is false-ish is unregistered, which is how
    /// the default log handler is turned off.
    pub fn update_configuration(&mut self, config: &Object, logger: &mut Logger) {
        for (name, value) in config {
            if !truthy(value) {
                self.unregister(name);
                continue;
            }
            match build(value, logger) {
                Ok(handler) => self.register(name, handler),
                // Upstream lets this escape as a `KeyError` or `TypeError` and
                // takes the whole boot stage with it (COMPAT.md B24).
                Err(why) => eprintln!("Ignoring reporting handler '{name}': {why}"),
            }
        }
    }
}

fn build(value: &Value, logger: &mut Logger) -> Result<Box<dyn Handler>, String> {
    let Some(config) = value.as_object() else {
        return Err("configuration is not a mapping".to_owned());
    };
    let Some(kind) = config.get("type") else {
        return Err("no 'type' given".to_owned());
    };
    let Some(kind) = kind.as_str() else {
        return Err(format!("'type' is not a string: {kind}"));
    };
    let handler: Box<dyn Handler> = match kind {
        "log" => {
            let level = config
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("DEBUG");
            Box::new(LogHandler::new(level, logger))
        }
        "print" => Box::new(PrintHandler),
        "hyperv" => {
            let path = config
                .get("kvp_file_path")
                .and_then(Value::as_str)
                .unwrap_or(kvp::POOL_FILE_GUEST);
            let event_types =
                config
                    .get("event_types")
                    .and_then(Value::as_array)
                    .map(|types| {
                        types
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    });
            Box::new(kvp::HyperVKvpHandler::new(
                Path::new(path),
                event_types,
                logger,
            ))
        }
        other => return Err(format!("unknown handler type '{other}'")),
    };
    for key in config.keys() {
        let known = matches!(
            (kind, key.as_str()),
            (_, "type")
                | ("log", "level")
                | ("hyperv", "kvp_file_path" | "event_types")
        );
        if !known {
            return Err(format!("unexpected key '{key}' for handler type '{kind}'"));
        }
    }
    Ok(handler)
}

/// Python truthiness, which is what upstream tests the handler config with.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::events::{EventStack, Status};

    #[derive(Debug, Clone, Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl Capture {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Handler for Capture {
        fn publish(&mut self, event: &events::Event, _logger: &mut Logger) {
            self.0.lock().unwrap().push(event.as_string());
        }
    }

    fn config(text: &str) -> Object {
        match ci_config::load_yaml(text, ci_config::Limits::default()).unwrap() {
            Value::Object(map) => map,
            other => panic!("not a mapping: {other}"),
        }
    }

    fn logger() -> Logger {
        Logger::silent()
    }

    fn reporter_with(capture: &Capture) -> Reporter {
        let mut reporter = Reporter::silent();
        reporter.register("capture", Box::new(capture.clone()));
        reporter
    }

    #[test]
    fn a_scope_reports_a_start_then_a_finish() {
        let capture = Capture::default();
        let mut reporter = reporter_with(&capture);

        let mut stack =
            EventStack::new("init-local", "searching for local datasources");
        stack.open(&mut reporter, &mut logger());
        stack.close(&mut reporter, &mut logger());

        let lines = capture.lines();
        assert_eq!(
            lines[0],
            "start: init-local: searching for local datasources"
        );
        assert!(
            lines[1].starts_with("finish: init-local: SUCCESS: searching for local datasources (duration: "),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn a_disabled_scope_reports_nothing() {
        let capture = Capture::default();
        let mut reporter = reporter_with(&capture);

        let mut stack = EventStack::new("n", "d").with_reporting(false);
        stack.open(&mut reporter, &mut logger());
        stack.close(&mut reporter, &mut logger());

        assert!(capture.lines().is_empty());
    }

    #[test]
    fn the_finish_event_reports_the_description_the_scope_ended_with() {
        let capture = Capture::default();
        let mut reporter = reporter_with(&capture);

        let mut stack =
            EventStack::new("check-cache", "attempting to read from cache [check]");
        stack.open(&mut reporter, &mut logger());
        stack.set_description("no cache found");
        stack.close(&mut reporter, &mut logger());

        assert!(capture.lines()[1].contains("SUCCESS: no cache found"));
    }

    #[test]
    fn a_false_ish_entry_unregisters_the_handler_of_that_name() {
        for off in [
            "logging: null",
            "logging: false",
            "logging: {}",
            "logging: ''",
        ] {
            let mut reporter = Reporter::default();
            reporter.update_configuration(&config(off), &mut logger());
            assert!(reporter.handlers.is_empty(), "{off} left a handler");
        }
    }

    #[test]
    fn re_registering_a_name_replaces_the_handler_and_moves_it_last() {
        let mut reporter = Reporter::default();
        reporter.update_configuration(
            &config("a: {type: print}\nlogging: {type: print}\n"),
            &mut logger(),
        );

        let names: Vec<&str> =
            reporter.handlers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "logging"]);
    }

    #[test]
    fn a_handler_that_cannot_be_built_is_skipped_rather_than_fatal() {
        for broken in [
            "h: {type: nosuch}",
            "h: {level: DEBUG}",
            "h: {type: print, bogus: 1}",
            "h: 5",
            "h: {type: 5}",
        ] {
            let mut reporter = Reporter::silent();
            reporter.update_configuration(&config(broken), &mut logger());
            assert!(
                reporter.handlers.is_empty(),
                "{broken} registered a handler"
            );
        }
    }

    #[test]
    fn a_log_handler_takes_a_level_and_a_print_handler_takes_nothing() {
        let mut reporter = Reporter::silent();
        reporter.update_configuration(
            &config("a: {type: log, level: WARN}\nb: {type: print}\n"),
            &mut logger(),
        );
        assert_eq!(reporter.handlers.len(), 2);
    }

    #[test]
    fn a_child_scope_carries_its_failure_up_to_the_parent() {
        let capture = Capture::default();
        let mut reporter = reporter_with(&capture);

        let mut parent = EventStack::new("init", "d");
        parent.open(&mut reporter, &mut logger());
        let mut child = parent.child("check-cache", "reading");
        child.open(&mut reporter, &mut logger());
        child.set_result(Status::Fail);
        let result = child.close(&mut reporter, &mut logger());
        parent.record_child(result);
        parent.close(&mut reporter, &mut logger());

        let lines = capture.lines();
        assert!(lines[1].starts_with("start: init/check-cache: reading"));
        assert!(lines[2].contains("finish: init/check-cache: FAIL"));
        assert!(lines[3].contains("finish: init: FAIL"));
    }
}
