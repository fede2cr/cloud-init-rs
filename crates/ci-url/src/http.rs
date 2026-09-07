//! A minimal HTTP/1.1 client.
//!
//! Upstream uses `requests`; nothing in the dependency set can stand in for it
//! at the declared MSRV — the current `ureq` release requires a newer
//! toolchain — so this is the small part of HTTP that the datasources need,
//! written to be boring: GET, bodyless PUT and POST, no cookies, no proxies, no
//! authentication, `identity` transfer only, and hard caps on every buffer,
//! because the responses come from wherever user-data points.
//!
//! `https` is OpenSSL, in `tls`.

use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use openssl::ssl::SslStream;

use crate::tls::SslDetails;
use crate::url::{self, Url};
use crate::{Error, Response};

/// Longest status line or header line accepted.
const MAX_LINE: u64 = 16 * 1024;
/// Most header lines accepted in one response.
const MAX_HEADERS: usize = 128;

pub(crate) struct Limits {
    pub timeout: Duration,
    pub max_bytes: u64,
    pub max_redirects: u32,
    pub headers: Vec<(String, String)>,
    pub method: crate::Method,
    pub body: Vec<u8>,
    pub unix_socket: Option<std::path::PathBuf>,
    pub ssl: SslDetails,
}

/// The ways to reach a server. The HTTP above them is identical; only the
/// dialling differs.
enum Stream {
    Tcp(TcpStream),
    Tls(Box<SslStream<TcpStream>>),
    Unix(UnixStream),
}

impl std::io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl std::io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
            Self::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Tls(s) => s.flush(),
            Self::Unix(s) => s.flush(),
        }
    }
}

pub(crate) fn get(start: &Url, limits: &Limits) -> Result<Response, Error> {
    let mut url = start.clone();
    for _ in 0..=limits.max_redirects {
        let response = get_once(&url, limits)?;
        let Some(location) = redirect_target(&response) else {
            return Ok(response);
        };
        let Some(next) = url::join(&url, &location) else {
            return Err(Error::new(
                url.to_string_full(),
                format!("invalid redirect target: {location}"),
            ));
        };
        url = next;
    }
    Err(Error::new(
        start.to_string_full(),
        format!("exceeded {} redirects", limits.max_redirects),
    ))
}

fn redirect_target(response: &Response) -> Option<String> {
    if !matches!(response.code, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    response.header("location").map(str::to_owned)
}

fn get_once(url: &Url, limits: &Limits) -> Result<Response, Error> {
    if url.scheme != "http" && url.scheme != "https" {
        return Err(Error::new(
            url.to_string_full(),
            format!("unsupported scheme: {}", url.scheme),
        ));
    }
    let fail = |message: String| Error::new(url.to_string_full(), message);
    let mut stream = connect(url, limits)?;

    let request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n\
         Accept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n{}{}\r\n",
        limits.method.as_str(),
        url.target,
        url.host_header(),
        ci_core::version::agent(),
        // `requests` gives anything but a GET an explicit length.
        if limits.method == crate::Method::Get {
            String::new()
        } else {
            format!("Content-Length: {}\r\n", limits.body.len())
        },
        extra_headers(&limits.headers),
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.write_all(&limits.body))
        .and_then(|()| stream.flush())
        .map_err(|e| fail(format!("write: {e}")))?;

    let mut reader = BufReader::new(stream);
    let code = read_status(&mut reader).map_err(&fail)?;
    let headers = read_headers(&mut reader).map_err(&fail)?;
    let contents = read_body(&mut reader, &headers, limits.max_bytes).map_err(&fail)?;
    Ok(Response {
        url: url.to_string_full(),
        code,
        headers,
        contents,
    })
}

/// Caller-supplied headers, each already terminated. A name or value carrying
/// a control character is dropped, so it cannot splice a header or a body.
fn extra_headers(headers: &[(String, String)]) -> String {
    let sane =
        |text: &str| !text.is_empty() && !text.bytes().any(|b| b < 0x20 || b == 0x7f);
    headers
        .iter()
        .filter(|(name, value)| sane(name) && sane(value) && !name.contains(':'))
        .fold(String::new(), |mut out, (name, value)| {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
            out
        })
}

fn connect(url: &Url, limits: &Limits) -> Result<Stream, Error> {
    let fail = |message: String| Error::new(url.to_string_full(), message);
    if let Some(path) = &limits.unix_socket {
        return dial_unix(path, limits.timeout).map_err(|e| fail(e.to_string()));
    }
    let tcp = dial_tcp(url, limits.timeout).map_err(|e| fail(e.to_string()))?;
    if url.scheme != "https" {
        return Ok(Stream::Tcp(tcp));
    }
    match crate::tls::connect(tcp, &url.host, &limits.ssl) {
        Ok(stream) => Ok(Stream::Tls(Box::new(stream))),
        Err(message) => Err(Error {
            tls: true,
            ..fail(message)
        }),
    }
}

fn dial_unix(path: &std::path::Path, timeout: Duration) -> std::io::Result<Stream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(Stream::Unix(stream))
}

fn dial_tcp(url: &Url, timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last = None;
    // Name resolution itself is not bounded: the system resolver has no
    // timeout knob short of running it on another thread.
    for addr in (url.host.as_str(), url.port_or_default()).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no address for host")
    }))
}

fn read_line<R: Read>(reader: &mut BufReader<R>) -> Result<String, String> {
    let mut line = Vec::new();
    reader
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .map_err(|e| format!("read: {e}"))?;
    if line.len() as u64 >= MAX_LINE {
        return Err("header line too long".to_owned());
    }
    String::from_utf8(line).map_err(|_| "header line is not utf-8".to_owned())
}

fn read_status<R: Read>(reader: &mut BufReader<R>) -> Result<u16, String> {
    let line = read_line(reader)?;
    let line = line.trim_end();
    let mut fields = line.splitn(3, ' ');
    match (fields.next(), fields.next()) {
        (Some(version), Some(code)) if version.starts_with("HTTP/") => code
            .parse()
            .map_err(|_| format!("unparseable status line: {line}")),
        _ => Err(format!("unparseable status line: {line}")),
    }
}

fn read_headers<R: Read>(
    reader: &mut BufReader<R>,
) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Ok(headers);
        }
        if headers.len() >= MAX_HEADERS {
            return Err("too many headers".to_owned());
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
}

fn read_body<R: Read>(
    reader: &mut BufReader<R>,
    headers: &[(String, String)],
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return read_chunked(reader, max_bytes);
    }
    let mut body = Vec::new();
    match find("content-length").and_then(|v| v.parse::<u64>().ok()) {
        Some(len) => {
            if len > max_bytes {
                return Err(format!(
                    "response of {len} bytes exceeds the {max_bytes} byte cap"
                ));
            }
            reader
                .take(len)
                .read_to_end(&mut body)
                .map_err(|e| format!("read: {e}"))?;
            if body.len() as u64 != len {
                return Err("response ended before Content-Length".to_owned());
            }
        }
        // No length and no chunking: the body runs to end of connection.
        None => {
            read_capped(reader.take(max_bytes + 1), &mut body, max_bytes)?;
        }
    }
    Ok(body)
}

fn read_chunked<R: Read>(
    reader: &mut BufReader<R>,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader)?;
        let size = line.trim_end();
        let size = size.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size, 16)
            .map_err(|_| format!("bad chunk size: {size}"))?;
        if size == 0 {
            // Trailers, then the terminating blank line.
            while !read_line(reader)?.trim_end().is_empty() {}
            return Ok(body);
        }
        if body.len() as u64 + size > max_bytes {
            return Err(format!("response exceeds the {max_bytes} byte cap"));
        }
        let start = body.len();
        reader
            .take(size)
            .read_to_end(&mut body)
            .map_err(|e| format!("read: {e}"))?;
        if (body.len() - start) as u64 != size {
            return Err("connection closed mid-chunk".to_owned());
        }
        let terminator = read_line(reader)?;
        if !terminator.trim_end().is_empty() {
            return Err("chunk not terminated by CRLF".to_owned());
        }
    }
}

fn read_capped(
    mut source: impl std::io::Read,
    into: &mut Vec<u8>,
    max_bytes: u64,
) -> Result<(), String> {
    source.read_to_end(into).map_err(|e| format!("read: {e}"))?;
    if into.len() as u64 > max_bytes {
        return Err(format!("response exceeds the {max_bytes} byte cap"));
    }
    Ok(())
}
