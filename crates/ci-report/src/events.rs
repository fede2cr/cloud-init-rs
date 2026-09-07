//! Reporting events: what happened, and how it is rendered.
//!
//! Port of `cloudinit/reporting/events.py`.

use std::time::Instant;

/// The result a finished event carries. `FAIL` outranks `WARN` when a parent
/// inherits from its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Success,
    Warn,
    Fail,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Start,
    Finish,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Finish => "finish",
        }
    }
}

/// A single reportable event. A finish event additionally carries the result
/// and how long the scope took.
///
/// `timestamp` is `time.time()` at construction, as `ReportingEvent.__init__`
/// defaults it. Only the Hyper-V KVP handler reads it; the log handler does not.
#[derive(Debug, Clone)]
pub struct Event {
    pub event_type: EventType,
    pub name: String,
    pub description: String,
    pub result: Option<Status>,
    pub duration: Option<f64>,
    pub timestamp: f64,
}

impl Event {
    pub fn as_string(&self) -> String {
        match (self.result, self.duration) {
            (Some(result), Some(duration)) => format!(
                "{}: {}: {}: {} (duration: {duration:.3}s)",
                self.event_type.as_str(),
                self.name,
                result.as_str(),
                self.description,
            ),
            _ => format!(
                "{}: {}: {}",
                self.event_type.as_str(),
                self.name,
                self.description
            ),
        }
    }
}

/// Port of `ReportEventStack`.
///
/// Upstream is a context manager reporting to a process-wide handler registry.
/// Reporting here is explicit, so the scope is opened and closed by hand and
/// the reporter is passed in.
#[derive(Debug)]
pub struct EventStack {
    fullname: String,
    description: String,
    message: Option<String>,
    result: Status,
    worst_child: Option<Status>,
    enabled: bool,
    started: Option<Instant>,
}

impl EventStack {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            fullname: name.to_owned(),
            description: description.to_owned(),
            message: None,
            result: Status::Success,
            worst_child: None,
            enabled: true,
            started: None,
        }
    }

    #[must_use]
    pub fn with_reporting(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// A nested scope, reported as `<parent>/<name>` and inheriting whether
    /// reporting is on at all.
    #[must_use]
    pub fn child(&self, name: &str, description: &str) -> Self {
        Self {
            fullname: format!("{}/{name}", self.fullname),
            enabled: self.enabled,
            ..Self::new(name, description)
        }
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    /// The finish event reports whatever the description says when the scope
    /// closes, not what it said when it opened.
    pub fn set_description(&mut self, description: impl Into<String>) {
        self.description = description.into();
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some(message.into());
    }

    pub fn set_result(&mut self, result: Status) {
        self.result = result;
    }

    /// Fold a closed child's result into this scope, which is how a parent ends
    /// up failing because something below it did.
    pub fn record_child(&mut self, result: Status) {
        if result > Status::Success && Some(result) > self.worst_child {
            self.worst_child = Some(result);
        }
    }

    fn message(&self) -> &str {
        self.message.as_deref().unwrap_or(&self.description)
    }

    pub fn open(
        &mut self,
        reporter: &mut crate::Reporter,
        logger: &mut ci_log::Logger,
    ) {
        self.result = Status::Success;
        self.started = Some(Instant::now());
        if self.enabled {
            reporter.publish(
                &Event {
                    event_type: EventType::Start,
                    name: self.fullname.clone(),
                    description: self.description.clone(),
                    result: None,
                    duration: None,
                    timestamp: ci_core::time::now_epoch(),
                },
                logger,
            );
        }
    }

    pub fn close(
        &mut self,
        reporter: &mut crate::Reporter,
        logger: &mut ci_log::Logger,
    ) -> Status {
        let result = self.worst_child.unwrap_or(self.result);
        if self.enabled {
            let duration = self
                .started
                .map_or(0.0, |start| start.elapsed().as_secs_f64());
            reporter.publish(
                &Event {
                    event_type: EventType::Finish,
                    name: self.fullname.clone(),
                    description: self.message().to_owned(),
                    result: Some(result),
                    duration: Some(duration),
                    timestamp: ci_core::time::now_epoch(),
                },
                logger,
            );
        }
        result
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

    fn finish(result: Status, duration: f64) -> Event {
        Event {
            event_type: EventType::Finish,
            name: "n".to_owned(),
            description: "d".to_owned(),
            result: Some(result),
            duration: Some(duration),
            timestamp: 0.0,
        }
    }

    #[test]
    fn a_start_event_reads_type_name_description() {
        let event = Event {
            event_type: EventType::Start,
            name: "init-local/check-cache".to_owned(),
            description: "attempting to read from cache [check]".to_owned(),
            result: None,
            duration: None,
            timestamp: 0.0,
        };
        assert_eq!(
            event.as_string(),
            "start: init-local/check-cache: attempting to read from cache [check]"
        );
    }

    #[test]
    fn a_finish_event_appends_the_result_and_a_three_decimal_duration() {
        assert_eq!(
            finish(Status::Fail, 12.0).as_string(),
            "finish: n: FAIL: d (duration: 12.000s)"
        );
        // Python's `{:.3f}` rounds the binary value, not the literal, so these
        // near-ties are where a naive `(x * 1000).round()` would disagree.
        for (duration, expected) in [
            (0.0535, "0.053"),
            (0.0545, "0.054"),
            (0.000_5, "0.001"),
            (1.2345, "1.234"),
        ] {
            assert_eq!(
                finish(Status::Success, duration).as_string(),
                format!("finish: n: SUCCESS: d (duration: {expected}s)")
            );
        }
    }

    #[test]
    fn a_failing_child_outranks_a_warning_one() {
        let mut parent = EventStack::new("p", "d");
        parent.record_child(Status::Warn);
        parent.record_child(Status::Fail);
        parent.record_child(Status::Success);

        let mut reporter = crate::Reporter::silent();
        let mut logger = ci_log::Logger::silent();
        assert_eq!(parent.close(&mut reporter, &mut logger), Status::Fail);
    }

    #[test]
    fn a_successful_child_does_not_overwrite_the_scopes_own_failure() {
        let mut stack = EventStack::new("p", "d");
        stack.set_result(Status::Fail);
        stack.record_child(Status::Success);

        let mut reporter = crate::Reporter::silent();
        let mut logger = ci_log::Logger::silent();
        assert_eq!(stack.close(&mut reporter, &mut logger), Status::Fail);
    }

    #[test]
    fn a_child_is_named_under_its_parent_and_inherits_reporting() {
        let parent = EventStack::new("init-local", "d").with_reporting(false);
        let child = parent.child("check-cache", "reading");
        assert_eq!(child.fullname(), "init-local/check-cache");
        assert!(!child.enabled);
    }
}
