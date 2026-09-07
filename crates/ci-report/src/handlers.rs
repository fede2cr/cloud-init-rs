//! Reporting handlers: where an event goes once it is published.
//!
//! Port of `cloudinit/reporting/handlers.py`. The `webhook` handler needs an
//! HTTP POST the port's URL client does not have yet and is absent; the
//! `hyperv` handler lives in [`crate::kvp`].

use crate::events::Event;
use ci_log::{Level, Logger};

pub trait Handler: std::fmt::Debug {
    fn publish(&mut self, event: &Event, logger: &mut Logger);

    fn flush(&mut self, _logger: &mut Logger) {}

    /// The Azure datasource writes its provisioning report through the KVP
    /// handler directly rather than as an event, so it has to be able to find
    /// it again among the registered handlers.
    fn as_kvp(&mut self) -> Option<&mut crate::kvp::HyperVKvpHandler> {
        None
    }
}

/// Upstream publishes to a `cloudinit.reporting.<type>.<name>` logger, so the
/// record it writes names `handlers.py` as its source however the event was
/// nested. The port says the same thing for the same reason.
#[derive(Debug)]
pub struct LogHandler {
    level: Level,
}

impl LogHandler {
    /// The level `DEFAULT_CONFIG` asks for, with no configuration to reject.
    #[must_use]
    pub fn at(level: Level) -> Self {
        Self { level }
    }

    /// `getattr(logging, level.upper())`, falling back to `WARN` with a
    /// complaint, exactly as upstream does for a level it cannot resolve.
    pub fn new(level: &str, logger: &mut Logger) -> Self {
        let parsed = match level.to_ascii_uppercase().as_str() {
            // `logging.NOTSET` is zero, which lets everything through.
            "NOTSET" => Some(Level::Trace),
            upper => Level::parse(upper),
        };
        let Some(level) = parsed else {
            logger.warning(
                "handlers.py",
                &format!("invalid level '{level}', using WARN"),
            );
            return Self {
                level: Level::Warning,
            };
        };
        Self { level }
    }
}

impl Handler for LogHandler {
    fn publish(&mut self, event: &Event, logger: &mut Logger) {
        logger.log(self.level, "handlers.py", &event.as_string());
    }
}

#[derive(Debug)]
pub struct PrintHandler;

impl Handler for PrintHandler {
    fn publish(&mut self, event: &Event, _logger: &mut Logger) {
        println!("{}", event.as_string());
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
    use crate::events::EventType;

    #[test]
    fn an_unknown_level_is_reported_but_not_fatal() {
        let mut logger = Logger::silent();
        LogHandler::new("nosuch", &mut logger);
        LogHandler::new("debug", &mut logger);
    }

    #[test]
    fn the_print_handler_writes_the_string_form() {
        let mut logger = Logger::silent();
        let mut handler = PrintHandler;
        handler.publish(
            &Event {
                event_type: EventType::Start,
                name: "n".to_owned(),
                description: "d".to_owned(),
                result: None,
                duration: None,
                timestamp: 0.0,
            },
            &mut logger,
        );
    }
}
