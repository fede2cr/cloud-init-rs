//! URL splitting, in the shape `urllib.parse` hands to `url_helper`.
//!
//! Only the pieces `read_file_or_url` and the HTTP client actually consult are
//! modelled: the scheme decides the transport, and host/port/target build the
//! request line. Query and fragment are carried through untouched rather than
//! re-encoded, because upstream passes the string on to `requests` verbatim.

/// A split URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// Everything from the first `/` of the path onwards, query included.
    pub target: String,
}

impl Url {
    /// The port the scheme implies when the URL does not name one.
    pub fn default_port(&self) -> u16 {
        if self.scheme == "https" {
            443
        } else {
            80
        }
    }

    /// The port to connect to.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or_else(|| self.default_port())
    }

    /// `Host:` header value: the port is omitted when it is the default, and
    /// an IPv6 literal keeps the brackets it was written with.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) if port != self.default_port() => format!("{host}:{port}"),
            _ => host,
        }
    }

    pub fn to_string_full(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host_header(), self.target)
    }
}

/// The scheme of a URL, as `urlparse` reports it: empty when there is no `:`
/// before the first `/`, or when what precedes it is not a valid scheme.
pub fn scheme_of(url: &str) -> &str {
    let Some(colon) = url.find(':') else {
        return "";
    };
    let Some(head) = url.get(..colon) else {
        return "";
    };
    let mut chars = head.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if valid {
        head
    } else {
        ""
    }
}

/// The path component, for `file:` URLs and bare paths.
///
/// `urlparse` does not percent-decode, so neither does this: upstream opens
/// the path exactly as written.
pub fn path_of(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(slash) => rest.get(slash..).unwrap_or_default(),
            // `file://host` with no path: urlparse puts it all in netloc.
            None => "",
        },
        None => match scheme_of(url).len() {
            0 => url,
            len => url.get(len + 1..).unwrap_or_default(),
        },
    };
    rest.split(['?', '#']).next().unwrap_or_default().to_owned()
}

/// `_cleanurl`, reduced to what it does in practice: a URL with no scheme is
/// read as `http`, and the leading component becomes the host.
pub fn parse_http(url: &str) -> Option<Url> {
    let scheme = scheme_of(url);
    let (scheme, rest) = if scheme.is_empty() {
        ("http".to_owned(), url)
    } else {
        let rest = url.get(scheme.len() + 1..)?;
        (scheme.to_ascii_lowercase(), rest)
    };
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let (authority, target) = match rest.find(['/', '?', '#']) {
        Some(cut) => (rest.get(..cut)?, rest.get(cut..)?),
        None => (rest, ""),
    };
    // Credentials in the authority are dropped rather than sent: no caller
    // needs them, and forwarding them across a redirect is how they leak.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = split_port(authority)?;
    if host.is_empty() {
        return None;
    }
    let target = if target.is_empty() || target.starts_with(['?', '#']) {
        format!("/{target}")
    } else {
        target.to_owned()
    };
    Some(Url {
        scheme,
        host: host.to_owned(),
        port,
        target,
    })
}

fn split_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal: the colons inside the brackets are not a port marker.
        let (host, after) = rest.split_once(']')?;
        return match after.strip_prefix(':') {
            Some("") | None => Some((host, None)),
            Some(port) => Some((host, Some(port.parse().ok()?))),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, "")) => Some((host, None)),
        Some((host, port)) => Some((host, Some(port.parse().ok()?))),
        None => Some((authority, None)),
    }
}

/// `url_helper.combine_url`: append path segments to a base URL.
///
/// Upstream percent-quotes each segment with `/` and `:` left safe.
#[must_use]
pub fn combine_url(base: &str, add_ons: &[&str]) -> String {
    let mut url = base.to_owned();
    for add_on in add_ons {
        let (head, query) = url
            .split_once('?')
            .map_or((url.as_str(), None), |(head, rest)| {
                (head, Some(rest.to_owned()))
            });
        let mut combined = head.to_owned();
        if !combined.is_empty() && !combined.ends_with('/') {
            combined.push('/');
        }
        combined.push_str(&quote_segment(add_on));
        if let Some(query) = query {
            combined.push('?');
            combined.push_str(&query);
        }
        url = combined;
    }
    url
}

/// `urllib.parse.quote(value, safe="/:")`.
fn quote_segment(value: &str) -> String {
    fn hex(nibble: u8) -> char {
        char::from(match nibble {
            0..=9 => b'0' + nibble,
            _ => b'A' + nibble - 10,
        })
    }
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/:".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex(byte >> 4));
            out.push(hex(byte & 0x0f));
        }
    }
    out
}

/// Resolve a `Location:` value against the URL it was returned from.
pub fn join(base: &Url, location: &str) -> Option<Url> {
    if !scheme_of(location).is_empty() {
        return parse_http(location);
    }
    if let Some(rest) = location.strip_prefix("//") {
        return parse_http(&format!("{}://{rest}", base.scheme));
    }
    if location.starts_with('/') {
        return Some(Url {
            target: location.to_owned(),
            ..base.clone()
        });
    }
    let dir = base
        .target
        .rfind('/')
        .and_then(|slash| base.target.get(..=slash))
        .unwrap_or("/");
    Some(Url {
        target: format!("{dir}{location}"),
        ..base.clone()
    })
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

    #[test]
    fn a_url_without_a_scheme_is_read_as_http() {
        let url = parse_http("example.com/seed").unwrap();
        assert_eq!(url.scheme, "http");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.target, "/seed");
    }

    #[test]
    fn a_port_is_split_off_the_authority() {
        let url = parse_http("http://127.0.0.1:8080/a?b=1").unwrap();
        assert_eq!(url.host, "127.0.0.1");
        assert_eq!(url.port, Some(8080));
        assert_eq!(url.target, "/a?b=1");
        assert_eq!(url.host_header(), "127.0.0.1:8080");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_colons() {
        let url = parse_http("http://[::1]:8080/a").unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.port, Some(8080));
    }

    #[test]
    fn credentials_in_the_authority_are_dropped() {
        let url = parse_http("http://user:pass@example.com/a").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, None);
    }

    #[test]
    fn a_url_with_no_path_gets_a_root_target() {
        assert_eq!(parse_http("http://example.com").unwrap().target, "/");
        assert_eq!(parse_http("http://example.com?a").unwrap().target, "/?a");
    }

    #[test]
    fn a_scheme_needs_a_letter_first() {
        assert_eq!(scheme_of("http://x"), "http");
        assert_eq!(scheme_of("1http://x"), "");
        assert_eq!(scheme_of("/tmp/x"), "");
        assert_eq!(scheme_of("x"), "");
    }

    #[test]
    fn a_file_url_keeps_its_path_undecoded() {
        assert_eq!(path_of("file:///tmp/a%20b"), "/tmp/a%20b");
        assert_eq!(path_of("file://localhost/tmp/x"), "/tmp/x");
        assert_eq!(path_of("/tmp/x"), "/tmp/x");
        assert_eq!(path_of("file:/tmp/x"), "/tmp/x");
    }

    #[test]
    fn a_relative_location_resolves_against_the_request() {
        let base = parse_http("http://h/a/b").unwrap();
        assert_eq!(join(&base, "c").unwrap().target, "/a/c");
        assert_eq!(join(&base, "/c").unwrap().target, "/c");
        assert_eq!(join(&base, "//other/c").unwrap().host, "other");
        assert_eq!(join(&base, "http://o/c").unwrap().host, "o");
    }

    #[test]
    fn combining_adds_one_separator_between_segments() {
        assert_eq!(
            combine_url("http://h", &["openstack"]),
            "http://h/openstack"
        );
        assert_eq!(
            combine_url("http://h/", &["openstack"]),
            "http://h/openstack"
        );
        assert_eq!(
            combine_url("http://h", &["openstack", "latest", "meta_data.json"]),
            "http://h/openstack/latest/meta_data.json"
        );
    }

    #[test]
    fn combining_appends_before_the_query_and_quotes_the_segment() {
        assert_eq!(combine_url("http://h/a?q=1", &["b"]), "http://h/a/b?q=1");
        assert_eq!(combine_url("http://h", &["a b"]), "http://h/a%20b");
        assert_eq!(combine_url("http://h", &["a/b:c"]), "http://h/a/b:c");
    }
}
