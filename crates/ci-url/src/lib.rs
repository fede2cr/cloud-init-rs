//! `url_helper.read_file_or_url`: fetch a URL, or read a local path.
//!
//! Scoped to what `#include` and `#include-once` need. The retry loop, the
//! `ok()` status window and the file/URL dispatch follow upstream; the response
//! object is a plain struct rather than a `requests.Response` shim.

use std::time::Duration;

mod http;
mod tls;
pub mod url;

pub use tls::{fetch_ssl_details, SslDetails};

/// Largest response accepted, matching the cap on decompressed user-data.
///
/// Upstream reads a response of any size into memory. An `#include` URL comes
/// from user-data, so an endless response is a trivial way to exhaust a booting
/// machine's memory; see docs/COMPAT.md.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// `readurl`'s `request_method`, limited to the verbs a datasource uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    #[default]
    Get,
    /// EC2's `IMDSv2` issues its API token in reply to a `PUT`.
    Put,
    /// Azure's wireserver takes the health report as a `POST` body.
    Post,
}

impl Method {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Post => "POST",
        }
    }
}

/// `readurl`'s knobs, at the values `_do_include` passes.
#[derive(Debug, Clone)]
pub struct Config {
    pub timeout: Duration,
    pub retries: u32,
    pub sec_between: Duration,
    pub max_bytes: u64,
    pub max_redirects: u32,
    /// `readurl`'s `headers` argument. Names and values carrying control
    /// characters are dropped rather than escaped, so a caller cannot splice
    /// extra headers into the request.
    pub headers: Vec<(String, String)>,
    pub method: Method,
    /// `readurl`'s `data` argument. Only a `POST` carries one.
    pub body: Vec<u8>,
    /// When set, the request goes to this Unix socket instead of being
    /// resolved and dialled over TCP. `LXD` serves its API on
    /// `/dev/lxd/sock`, which upstream reaches by mounting a `requests`
    /// adapter over a `AF_UNIX` connection pool.
    pub unix_socket: Option<std::path::PathBuf>,
    /// `readurl`'s `ssl_details` argument, which only an `https` URL consults.
    pub ssl: SslDetails,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            retries: 10,
            sec_between: Duration::from_secs(1),
            max_bytes: MAX_BYTES,
            // `requests.Session` stops after 30.
            max_redirects: 30,
            headers: Vec::new(),
            method: Method::Get,
            body: Vec::new(),
            unix_socket: None,
            ssl: SslDetails::default(),
        }
    }
}

/// A fetched response. `FileResponse` and `UrlResponse` collapse into one type
/// because every caller treats them the same way.
#[derive(Debug, Clone)]
pub struct Response {
    pub url: String,
    pub code: u16,
    pub headers: Vec<(String, String)>,
    pub contents: Vec<u8>,
}

impl Response {
    /// `UrlResponse.ok()` with the default `redirects_ok=False`.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }

    /// Header lookup by lower-cased name; the client stores them folded.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// `UrlError`. The status code is carried because the retry loop keys off it.
#[derive(Debug, Clone)]
pub struct Error {
    pub url: String,
    pub code: Option<u16>,
    pub message: String,
    /// Set when the TLS handshake is what failed. Upstream lets `SSLError`
    /// straight out of the retry loop: "ssl exceptions are not going to get
    /// fixed by waiting a few seconds".
    pub tls: bool,
}

impl Error {
    fn new(url: String, message: String) -> Self {
        Self {
            url,
            code: None,
            message,
            tls: false,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if !self.message.contains(&self.url) {
            write!(f, " for url: {}", self.url)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// `read_file_or_url`.
pub fn read_file_or_url(raw: &str, config: &Config) -> Result<Response, Error> {
    let raw = raw.trim_start();
    let scheme = url::scheme_of(raw);
    if scheme == "file" || raw.starts_with('/') {
        return read_file(&url::path_of(raw), config.max_bytes);
    }
    if scheme == "ftp" || scheme == "ftps" {
        return Err(Error::new(
            raw.to_owned(),
            "ftp is not supported by this port".to_owned(),
        ));
    }
    readurl(raw, config)
}

/// `_read_file`. A missing file is a 404, as it is upstream.
pub fn read_file(path: &str, max_bytes: u64) -> Result<Response, Error> {
    match ci_sys::path::read_capped(path, max_bytes) {
        Ok(contents) => Ok(Response {
            url: path.to_owned(),
            code: 200,
            headers: Vec::new(),
            contents,
        }),
        Err(e) => {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                Some(404)
            } else {
                None
            };
            Err(Error {
                url: path.to_owned(),
                code,
                message: format!("{e}: '{path}'"),
                tls: false,
            })
        }
    }
}

/// `readurl`'s retry loop, with `check_status=True`: a response outside the
/// `ok()` window is an error to be retried, not a value to return.
///
/// Upstream retries a 503 for as long as the endpoint keeps sending them,
/// ignoring `retries` entirely; here the attempt budget is the budget. See
/// docs/COMPAT.md.
pub fn readurl(raw: &str, config: &Config) -> Result<Response, Error> {
    let Some(parsed) = url::parse_http(raw) else {
        return Err(Error::new(raw.to_owned(), "unparseable url".to_owned()));
    };
    let limits = http::Limits {
        timeout: config.timeout,
        max_bytes: config.max_bytes,
        max_redirects: config.max_redirects,
        headers: config.headers.clone(),
        method: config.method,
        body: config.body.clone(),
        unix_socket: config.unix_socket.clone(),
        ssl: config.ssl.clone(),
    };
    let attempts = config.retries.saturating_add(1);
    let mut last =
        Error::new(parsed.to_string_full(), "no attempt was made".to_owned());
    for attempt in 0..attempts {
        match http::get(&parsed, &limits) {
            Ok(response) if response.ok() => return Ok(response),
            Ok(response) => {
                last = Error {
                    url: response.url,
                    code: Some(response.code),
                    message: format!("{} Client Error", response.code),
                    tls: false,
                };
            }
            Err(e) if e.tls => return Err(e),
            Err(e) => last = e,
        }
        if attempt + 1 < attempts && !config.sec_between.is_zero() {
            std::thread::sleep(config.sec_between);
        }
    }
    Err(last)
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

    fn no_retries() -> Config {
        Config {
            retries: 0,
            sec_between: Duration::ZERO,
            ..Config::default()
        }
    }

    #[test]
    fn a_file_url_and_a_bare_path_read_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed");
        std::fs::write(&path, b"hello").unwrap();
        let path = path.to_str().unwrap();

        for url in [path.to_owned(), format!("file://{path}")] {
            let response = read_file_or_url(&url, &no_retries()).unwrap();
            assert!(response.ok());
            assert_eq!(response.contents, b"hello");
        }
    }

    #[test]
    fn a_missing_file_is_a_404() {
        let err = read_file_or_url("/nonexistent/seed", &no_retries()).unwrap_err();
        assert_eq!(err.code, Some(404));
    }

    #[test]
    fn leading_whitespace_is_stripped_before_the_scheme_is_read() {
        let err =
            read_file_or_url("  ftp://example.invalid/a", &no_retries()).unwrap_err();
        assert!(err.message.contains("ftp"), "{err}");
    }

    #[test]
    fn an_error_display_names_the_url_when_the_message_does_not() {
        let err = Error::new("http://h/a".to_owned(), "boom".to_owned());
        assert_eq!(err.to_string(), "boom for url: http://h/a");
        let err = Error::new("http://h/a".to_owned(), "boom http://h/a".to_owned());
        assert_eq!(err.to_string(), "boom http://h/a");
    }

    /// Answer `replies.len()` requests with the given raw responses, in order.
    fn serve(replies: Vec<&'static str>) -> String {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn a_content_length_body_is_read_whole() {
        let base = serve(vec!["HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"]);
        let response =
            read_file_or_url(&format!("{base}/seed"), &no_retries()).unwrap();
        assert_eq!(response.code, 200);
        assert_eq!(response.contents, b"hello");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        let base = serve(vec![
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
             3\r\nhel\r\n2\r\nlo\r\n0\r\n\r\n",
        ]);
        let response =
            read_file_or_url(&format!("{base}/seed"), &no_retries()).unwrap();
        assert_eq!(response.contents, b"hello");
    }

    #[test]
    fn a_body_with_no_length_runs_to_end_of_connection() {
        let base = serve(vec!["HTTP/1.1 200 OK\r\n\r\nhello"]);
        let response =
            read_file_or_url(&format!("{base}/seed"), &no_retries()).unwrap();
        assert_eq!(response.contents, b"hello");
    }

    #[test]
    fn a_redirect_is_followed() {
        let base = serve(vec![
            "HTTP/1.1 302 Found\r\nLocation: /moved\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nthere",
        ]);
        let response =
            read_file_or_url(&format!("{base}/seed"), &no_retries()).unwrap();
        assert_eq!(response.contents, b"there");
        assert!(response.url.ends_with("/moved"), "{}", response.url);
    }

    #[test]
    fn a_response_larger_than_the_cap_is_refused() {
        let base = serve(vec!["HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"]);
        let config = Config {
            max_bytes: 4,
            ..no_retries()
        };
        let err = read_file_or_url(&format!("{base}/seed"), &config).unwrap_err();
        assert!(err.message.contains("cap"), "{err}");
    }

    #[test]
    fn a_404_is_an_error_carrying_its_status() {
        let base = serve(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
        let err = read_file_or_url(&format!("{base}/seed"), &no_retries()).unwrap_err();
        assert_eq!(err.code, Some(404));
    }

    #[test]
    fn a_failed_attempt_is_retried_up_to_the_budget() {
        let base = serve(vec![
            "HTTP/1.1 500 Server Error\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let config = Config {
            retries: 1,
            sec_between: Duration::ZERO,
            ..Config::default()
        };
        let response = read_file_or_url(&format!("{base}/seed"), &config).unwrap();
        assert_eq!(response.contents, b"ok");
    }
}
