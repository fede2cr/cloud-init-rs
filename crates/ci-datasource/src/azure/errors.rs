//! Port of `sources/azure/errors.py`: the provisioning failure taxonomy and the
//! CSV encoding Azure's host agent reads it back out of.

use ci_core::time;

const DOCUMENTATION_URL: &str = "https://aka.ms/linuxprovisioningerror";

/// One failure, in the shape the host expects to be told about it.
///
/// `supporting_data` is a list rather than a map because upstream builds it as
/// an insertion-ordered `dict` and the report is compared as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportableError {
    pub reason: String,
    pub supporting_data: Vec<(String, String)>,
    pub timestamp: String,
}

impl ReportableError {
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            supporting_data: Vec::new(),
            timestamp: time::format_python_isoformat_utc(time::now_epoch()),
        }
    }

    #[must_use]
    fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.supporting_data.push((key.to_owned(), value.into()));
        self
    }

    /// The `PROVISIONING_REPORT` payload.
    #[must_use]
    pub fn as_encoded_report(&self, vm_id: Option<&str>) -> String {
        let mut fields = vec![
            "result=error".to_owned(),
            format!("reason={}", self.reason),
            format!("agent={}", agent()),
        ];
        for (key, value) in &self.supporting_data {
            fields.push(format!("{key}={value}"));
        }
        fields.push(format!("vm_id={}", show(vm_id)));
        fields.push(format!("timestamp={}", self.timestamp));
        fields.push(format!("documentation_url={DOCUMENTATION_URL}"));
        encode_report(&fields)
    }
}

/// Upstream's `f"Cloud-Init/{version_string()}"`, renamed so the platform can
/// tell the implementations apart (deviation 102).
#[must_use]
pub fn agent() -> String {
    ci_core::version::agent()
}

/// `csv.writer(delimiter="|", quotechar="'", quoting=QUOTE_MINIMAL)` over one
/// row, with the line terminator stripped again.
#[must_use]
pub fn encode_report(fields: &[String]) -> String {
    let row: Vec<String> = fields.iter().map(|field| quote(field)).collect();
    // Upstream's `rstrip()` takes the CRLF off, and any trailing whitespace of
    // the last field with it.
    row.join("|").trim_end().to_owned()
}

/// `QUOTE_MINIMAL`: quote only when the field would otherwise be ambiguous, and
/// double an embedded quote.
fn quote(field: &str) -> String {
    let needs = field.contains(['|', '\'', '\r', '\n']);
    if !needs {
        return field.to_owned();
    }
    format!("'{}'", field.replace('\'', "''"))
}

/// `str()` of an optional value, which is what the f-strings interpolate.
fn show(value: Option<&str>) -> &str {
    value.unwrap_or("None")
}

/// Python's `str(float)`, which keeps a trailing `.0` that Rust drops.
#[must_use]
pub fn py_float(value: f64) -> String {
    ci_core::jsonfmt::py_float(value)
}

#[must_use]
pub fn imds_url_error(error: &ci_url::Error, duration: f64) -> ReportableError {
    // Upstream picks one of three reasons by inspecting the `requests`
    // exception class behind the failure: connect timeout, connection error or
    // read timeout. `ci_url` does not classify its causes, so only the two
    // reasons that are decidable from an `Error` are produced; upstream lands
    // on the same "unexpected" wording whenever the cause is none of the three.
    let reason = match error.code {
        Some(code) => format!("http error {code} querying IMDS"),
        None => "unexpected error querying IMDS".to_owned(),
    };
    let mut report = ReportableError::new(reason);
    if let Some(code) = error.code {
        report = report.with("http_code", code.to_string());
    }
    report
        .with("duration", py_float(duration))
        .with("exception", &error.message)
        .with("url", &error.url)
}

#[must_use]
pub fn imds_invalid_metadata(key: &str, value: &ci_config::Value) -> ReportableError {
    ReportableError::new(format!("invalid IMDS metadata for key={key}"))
        .with("key", key)
        .with("value", python_str(value))
        .with("type", python_type_name(value))
}

#[must_use]
pub fn imds_metadata_parsing_exception(message: &str) -> ReportableError {
    ReportableError::new("error parsing IMDS metadata").with("exception", message)
}

#[must_use]
pub fn os_disk_pps_failure() -> ReportableError {
    ReportableError::new("error waiting for host shutdown")
}

#[must_use]
pub fn ovf_invalid_metadata(message: &str) -> ReportableError {
    ReportableError::new(format!(
        "unexpected metadata parsing ovf-env.xml: {message}"
    ))
}

#[must_use]
pub fn ovf_parsing_exception(message: &str) -> ReportableError {
    ReportableError::new(format!("error parsing ovf-env.xml: {message}"))
}

#[must_use]
pub fn proxy_agent_not_found() -> ReportableError {
    ReportableError::new("azure-proxy-agent not found")
}

#[must_use]
pub fn proxy_agent_status_failure(
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> ReportableError {
    ReportableError::new("azure-proxy-agent status failure")
        .with("exit_code", exit_code.to_string())
        .with("stdout", indent_text(stdout))
        .with("stderr", indent_text(stderr))
}

/// `ProcessExecutionError._indent_text`, which upstream applies to the stored
/// `stdout`/`stderr` and not just to the rendered message, so the reindented
/// copy is what reaches the report (bug B47).
fn indent_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.trim_end_matches('\n').replace('\n', "\n        ")
}

#[must_use]
pub fn vm_identification(message: &str, system_uuid: Option<&str>) -> ReportableError {
    ReportableError::new("failure to identify Azure VM ID")
        .with("exception", message)
        .with("system_uuid", show(system_uuid))
}

/// `ReportableErrorUnhandledException`, which is what `_get_data` wraps any
/// crawl failure that is not already reportable in.
///
/// Upstream also carries `traceback_base64`, the reversed Python traceback.
/// There is no traceback to carry here, so the field is left out rather than
/// filled with something the platform's tooling would try to decode
/// (COMPAT.md deviation 136).
#[must_use]
pub fn unhandled_exception(message: &str) -> ReportableError {
    ReportableError::new("unhandled exception").with("exception", message)
}

/// `type(value).__name__` for the JSON shapes IMDS can produce.
fn python_type_name(value: &ci_config::Value) -> &'static str {
    match value {
        ci_config::Value::Null => "NoneType",
        ci_config::Value::Bool(_) => "bool",
        ci_config::Value::Number(number) => {
            if number.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        ci_config::Value::String(_) => "str",
        ci_config::Value::Array(_) => "list",
        ci_config::Value::Object(_) => "dict",
    }
}

/// `str(value)`, which an f-string interpolates: bare for a string, `repr` for
/// everything else.
fn python_str(value: &ci_config::Value) -> String {
    match value {
        ci_config::Value::String(text) => text.clone(),
        other => python_repr(other),
    }
}

fn python_repr(value: &ci_config::Value) -> String {
    match value {
        ci_config::Value::Null => "None".to_owned(),
        ci_config::Value::Bool(true) => "True".to_owned(),
        ci_config::Value::Bool(false) => "False".to_owned(),
        ci_config::Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => py_float(float),
            _ => number.to_string(),
        },
        ci_config::Value::String(text) => quote_repr(text),
        ci_config::Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(python_repr).collect();
            format!("[{}]", rendered.join(", "))
        }
        ci_config::Value::Object(map) => {
            let rendered: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    format!("{}: {}", quote_repr(key), python_repr(value))
                })
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

/// Python prefers single quotes, switching to double only to avoid escaping.
fn quote_repr(text: &str) -> String {
    if text.contains('\'') && !text.contains('"') {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
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
    use super::{
        encode_report, imds_url_error, indent_text, ovf_invalid_metadata, py_float,
        ReportableError,
    };

    fn fields(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn plain_fields_are_joined_by_the_delimiter() {
        assert_eq!(
            encode_report(&fields(&["result=error", "reason=nope"])),
            "result=error|reason=nope"
        );
    }

    #[test]
    fn a_field_carrying_the_delimiter_is_quoted() {
        assert_eq!(encode_report(&fields(&["a=1|2", "b=3"])), "'a=1|2'|b=3");
    }

    #[test]
    fn an_embedded_quote_is_doubled_and_forces_quoting() {
        assert_eq!(encode_report(&fields(&["a=it's"])), "'a=it''s'");
    }

    #[test]
    fn a_newline_forces_quoting_because_it_ends_a_record() {
        assert_eq!(encode_report(&fields(&["a=1\n2"])), "'a=1\n2'");
    }

    #[test]
    fn trailing_whitespace_of_the_last_field_is_lost_to_rstrip() {
        assert_eq!(encode_report(&fields(&["a=1", "b=2  "])), "a=1|b=2");
    }

    #[test]
    fn a_report_carries_the_fixed_fields_in_order() {
        let mut error = ovf_invalid_metadata("missing configuration for 'HostName'");
        error.timestamp = "2026-09-02T00:00:00+00:00".to_owned();
        let report = error.as_encoded_report(Some("abc"));
        // The reason carries a quote, so the whole field is quoted and the
        // inner quotes are doubled.
        assert!(report.starts_with(
            "result=error|'reason=unexpected metadata parsing ovf-env.xml: \
             missing configuration for ''HostName'''|agent=Cloud-Init-rs/"
        ));
        assert!(report.ends_with(
            "|vm_id=abc|timestamp=2026-09-02T00:00:00+00:00\
             |documentation_url=https://aka.ms/linuxprovisioningerror"
        ));
    }

    #[test]
    fn a_missing_vm_id_is_the_word_none() {
        let mut error = ReportableError::new("nope");
        error.timestamp = "t".to_owned();
        assert!(error.as_encoded_report(None).contains("|vm_id=None|"));
    }

    #[test]
    fn integral_durations_keep_the_trailing_zero_python_prints() {
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(0.0), "0.0");
        assert_eq!(py_float(1.5), "1.5");
    }

    #[test]
    fn an_unclassified_url_failure_is_reported_as_unexpected() {
        let failed = ci_url::Error {
            url: "http://h/meta".to_owned(),
            code: None,
            message: "no response".to_owned(),
            tls: false,
        };
        let report = imds_url_error(&failed, 0.5);
        assert_eq!(report.reason, "unexpected error querying IMDS");
        assert!(!report
            .supporting_data
            .iter()
            .any(|(key, _)| key == "http_code"));

        let refused = ci_url::Error {
            code: Some(404),
            ..failed
        };
        let report = imds_url_error(&refused, 0.5);
        assert_eq!(report.reason, "http error 404 querying IMDS");
        assert_eq!(report.supporting_data[0].0, "http_code");
    }

    #[test]
    fn process_output_is_reindented_the_way_upstream_stores_it() {
        assert_eq!(indent_text("out\nline two"), "out\n        line two");
        assert_eq!(indent_text("out\n\n"), "out");
        assert_eq!(indent_text(""), "");
    }
}
