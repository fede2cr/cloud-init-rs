//! Port of `sources/azure/imds.py`. Azure's instance metadata service is
//! polled rather than merely read: early in boot it answers 404 or 410 until
//! the platform has provisioned, so the client retries indefinitely and decides
//! per-error whether to keep going.

use std::time::{Duration, Instant};

use ci_config::Value;

/// `IMDS_URL`.
pub const IMDS_URL: &str = "http://169.254.169.254/metadata";

/// `ReadUrlRetryHandler.retry_codes`: not-found-yet, gone-yet, throttled, and
/// server error.
const RETRY_CODES: &[u16] = &[404, 410, 429, 500];

/// `readurl`'s `sec_between` default, which every IMDS call takes.
const SEC_BETWEEN: Duration = Duration::from_secs(1);

/// `_fetch_url`'s `timeout` default.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What the previous failure was, so a repeat can be logged only once. Upstream
/// stores either the HTTP code or the exception's class; the port's client
/// distinguishes only "no response at all", so that whole class collapses here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastError {
    Code(u16),
    Connection,
}

/// `ReadUrlRetryHandler`.
#[derive(Debug)]
pub struct RetryHandler {
    logging_backoff: f64,
    max_connection_errors: Option<u32>,
    retry_deadline: Option<Instant>,
    logging_threshold: f64,
    request_count: u32,
    last_error: Option<LastError>,
}

impl RetryHandler {
    #[must_use]
    pub fn new(
        logging_backoff: f64,
        max_connection_errors: Option<u32>,
        retry_deadline: Option<Instant>,
    ) -> Self {
        Self {
            logging_backoff,
            max_connection_errors,
            retry_deadline,
            logging_threshold: 1.0,
            request_count: 0,
            last_error: None,
        }
    }

    /// How many requests have been attempted, for the reprovision tally.
    #[must_use]
    pub fn request_count(&self) -> u32 {
        self.request_count
    }

    /// `exception_callback`: whether to poll again, deciding on the way whether
    /// this failure is worth another log line.
    fn should_retry(
        &mut self,
        error: &ci_url::Error,
        log: &mut ci_log::Logger,
    ) -> bool {
        self.request_count = self.request_count.saturating_add(1);

        let mut retry = self
            .retry_deadline
            .is_none_or(|deadline| Instant::now() < deadline);

        if let (Some(remaining), None) = (self.max_connection_errors, error.code) {
            let remaining = remaining.saturating_sub(1);
            self.max_connection_errors = Some(remaining);
            if remaining == 0 {
                retry = false;
            }
        } else if error.code.is_some_and(|code| !RETRY_CODES.contains(&code)) {
            retry = false;
        }

        let mut should_log = f64::from(self.request_count) >= self.logging_threshold;
        if should_log {
            self.logging_threshold *= self.logging_backoff;
        }

        let seen = error.code.map_or(LastError::Connection, LastError::Code);
        if self.last_error != Some(seen) {
            should_log = true;
            self.last_error = Some(seen);
        }

        if should_log || !retry {
            log.warning(
                "azure.py",
                &format!(
                    "Polling IMDS failed attempt {} with exception: {}",
                    self.request_count, error.message
                ),
            );
        }
        retry
    }
}

/// `headers_cb`: a fresh correlation id per request, so the platform can tie
/// its own logs to one boot's polling.
fn headers() -> Vec<(String, String)> {
    vec![
        ("Metadata".to_owned(), "true".to_owned()),
        (
            "x-ms-client-request-id".to_owned(),
            ci_core::uuid::Uuid::v4().to_string(),
        ),
    ]
}

/// `_fetch_url`, with `readurl`'s `infinite=True` loop unrolled here because the
/// port's client has no exception callback.
fn fetch_url(
    url: &str,
    handler: &mut RetryHandler,
    log: &mut ci_log::Logger,
) -> Result<Vec<u8>, ci_url::Error> {
    loop {
        let config = ci_url::Config {
            timeout: TIMEOUT,
            retries: 0,
            sec_between: Duration::ZERO,
            headers: headers(),
            ..ci_url::Config::default()
        };
        match ci_url::readurl(url, &config) {
            Ok(response) => return Ok(response.contents),
            Err(error) => {
                if !handler.should_retry(&error, log) {
                    log.warning(
                        "azure.py",
                        &format!(
                            "Failed to fetch metadata from IMDS: {}",
                            error.message
                        ),
                    );
                    return Err(error);
                }
                std::thread::sleep(SEC_BETWEEN);
            }
        }
    }
}

/// `_fetch_metadata`.
fn fetch_metadata(
    url: &str,
    handler: &mut RetryHandler,
    log: &mut ci_log::Logger,
) -> Result<Value, ci_url::Error> {
    let raw = fetch_url(url, handler, log)?;
    let text = String::from_utf8_lossy(&raw);
    serde_json::from_str(&text).map_err(|error| {
        log.warning(
            "azure.py",
            &format!("Failed to parse metadata from IMDS: {error}"),
        );
        ci_url::Error {
            url: url.to_owned(),
            code: None,
            message: format!("Failed to parse metadata from IMDS: {error}"),
            tls: false,
        }
    })
}

/// `fetch_metadata_with_api_fallback`. The extended API carries the fields
/// cloud-init wants; an image on a host too old to serve it answers 400, which
/// is not a retry code and so escapes the poll immediately.
///
/// # Errors
/// The underlying fetch failed, or the body was not JSON.
pub fn fetch_metadata_with_api_fallback(
    base: &str,
    retry_deadline: Option<Instant>,
    max_connection_errors: Option<u32>,
    log: &mut ci_log::Logger,
) -> Result<Value, ci_url::Error> {
    let mut handler = RetryHandler::new(1.0, max_connection_errors, retry_deadline);
    let extended = format!("{base}/instance?api-version=2021-08-01&extended=true");
    match fetch_metadata(&extended, &mut handler, log) {
        Err(error) if error.code == Some(400) => {
            log.warning("azure.py", "Falling back to IMDS api-version: 2019-06-01");
            let mut handler =
                RetryHandler::new(1.0, max_connection_errors, retry_deadline);
            fetch_metadata(
                &format!("{base}/instance?api-version=2019-06-01"),
                &mut handler,
                log,
            )
        }
        other => other,
    }
}

/// `fetch_reprovision_data`: the blob a pre-provisioned VM waits on. It has no
/// deadline — the wait is the point — but it gives up after one connection
/// error, which early in boot means the wrong NIC.
///
/// # Errors
/// The fetch was abandoned.
pub fn fetch_reprovision_data(
    base: &str,
    log: &mut ci_log::Logger,
) -> Result<Vec<u8>, ci_url::Error> {
    let mut handler = RetryHandler::new(2.0, Some(1), None);
    let url = format!("{base}/reprovisiondata?api-version=2019-06-01");
    let contents = fetch_url(&url, &mut handler, log)?;
    log.debug(
        "azure.py",
        &format!(
            "Polled IMDS {} time(s)",
            handler.request_count().saturating_add(1)
        ),
    );
    Ok(contents)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{headers, RetryHandler};

    fn failure(code: Option<u16>) -> ci_url::Error {
        ci_url::Error {
            url: "http://169.254.169.254/metadata".to_owned(),
            code,
            message: "boom".to_owned(),
            tls: false,
        }
    }

    #[test]
    fn a_retry_code_is_polled_again_and_anything_else_is_not() {
        let mut log = ci_log::Logger::silent();
        for code in [404, 410, 429, 500] {
            let mut handler = RetryHandler::new(1.0, None, None);
            assert!(handler.should_retry(&failure(Some(code)), &mut log));
        }
        for code in [400, 403, 503] {
            let mut handler = RetryHandler::new(1.0, None, None);
            assert!(!handler.should_retry(&failure(Some(code)), &mut log));
        }
    }

    #[test]
    fn a_passed_deadline_stops_the_poll_even_for_a_retry_code() {
        let mut log = ci_log::Logger::silent();
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let mut handler = RetryHandler::new(1.0, None, Some(past));
        assert!(!handler.should_retry(&failure(Some(404)), &mut log));

        let future = Instant::now() + Duration::from_secs(60);
        let mut handler = RetryHandler::new(1.0, None, Some(future));
        assert!(handler.should_retry(&failure(Some(404)), &mut log));
    }

    #[test]
    fn connection_errors_are_counted_down_but_only_when_a_budget_was_given() {
        let mut log = ci_log::Logger::silent();
        let mut handler = RetryHandler::new(1.0, Some(2), None);
        assert!(handler.should_retry(&failure(None), &mut log));
        assert!(!handler.should_retry(&failure(None), &mut log));

        // No budget means a connection error polls forever.
        let mut handler = RetryHandler::new(1.0, None, None);
        for _ in 0..5 {
            assert!(handler.should_retry(&failure(None), &mut log));
        }
    }

    #[test]
    fn the_connection_budget_does_not_touch_http_failures() {
        let mut log = ci_log::Logger::silent();
        let mut handler = RetryHandler::new(1.0, Some(1), None);
        assert!(handler.should_retry(&failure(Some(404)), &mut log));
        assert!(handler.should_retry(&failure(Some(404)), &mut log));
        assert!(!handler.should_retry(&failure(None), &mut log));
    }

    #[test]
    fn every_request_is_counted_whatever_the_verdict() {
        let mut log = ci_log::Logger::silent();
        let mut handler = RetryHandler::new(1.0, None, None);
        handler.should_retry(&failure(Some(404)), &mut log);
        handler.should_retry(&failure(Some(400)), &mut log);
        assert_eq!(handler.request_count(), 2);
    }

    #[test]
    fn each_request_carries_a_distinct_correlation_id() {
        let first = headers();
        let second = headers();
        assert_eq!(first[0], ("Metadata".to_owned(), "true".to_owned()));
        assert_eq!(first[1].0, "x-ms-client-request-id");
        assert_ne!(first[1].1, second[1].1);
    }
}
