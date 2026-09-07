//! Which name this machine answers to: `util.get_hostname_fqdn` and the two
//! lookups underneath it.
//!
//! Three sources are consulted in a fixed order — the config, the datasource's
//! metadata, and the running system — and the interesting part is the
//! *precedence*, not any one lookup. A tenant who sets only `fqdn` gets a
//! short name derived from it; one who sets a dotted `hostname` gets both from
//! that; one who sets neither gets whatever the image booted with. Getting the
//! order wrong is how a machine ends up named `localhost` forever.

use std::path::Path;

use ci_config::{option, Object, Value};

/// The `localhost.localdomain` default upstream falls back to.
const DEFAULT_HOST: &str = "localhost";
const DEFAULT_DOMAIN: &str = "localdomain";

/// `util.HostnameFqdnInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameFqdn {
    pub hostname: String,
    pub fqdn: String,
    /// True only when the hostname is `localhost` *because nothing supplied
    /// one*. `cc_set_hostname` uses it to leave the name alone and let
    /// systemd decide, which a tenant who really asked for `localhost` does
    /// not get.
    pub is_default: bool,
}

/// `util.get_hostname_fqdn(cfg, cloud)`.
///
/// `metadata` is the datasource's, or `None` when there is no datasource;
/// `root` prefixes `/etc/hosts` and `/proc`.
#[must_use]
pub fn get_hostname_fqdn(
    cfg: &Object,
    metadata: Option<&Object>,
    root: &Path,
) -> HostnameFqdn {
    if let Some(fqdn) = cfg.get("fqdn") {
        let fqdn = py_str(fqdn);
        let hostname = option::get_str(cfg, "hostname").map_or_else(
            || fqdn.split('.').next().unwrap_or("").to_owned(),
            ToOwned::to_owned,
        );
        return HostnameFqdn {
            hostname,
            fqdn,
            is_default: false,
        };
    }

    // A dotted `hostname` is taken as both names. `find(".") > 0` is not the
    // same as `contains`: a leading dot does not count.
    if let Some(Value::String(configured)) = cfg.get("hostname") {
        if configured.find('.').is_some_and(|at| at > 0) {
            let (short, _) = configured.split_at(configured.find('.').unwrap_or(0));
            return HostnameFqdn {
                hostname: short.to_owned(),
                fqdn: configured.clone(),
                is_default: false,
            };
        }
    }

    let fqdn = datasource_hostname(metadata, true, root);
    if let Some(hostname) = option::get_str(cfg, "hostname") {
        return HostnameFqdn {
            hostname: hostname.to_owned(),
            fqdn: fqdn.hostname,
            is_default: false,
        };
    }
    let short = datasource_hostname(metadata, false, root);
    HostnameFqdn {
        hostname: short.hostname,
        fqdn: fqdn.hostname,
        is_default: short.is_default,
    }
}

/// `DataSourceHostname`, the answer `DataSource.get_hostname` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasourceHostname {
    pub hostname: String,
    pub is_default: bool,
}

/// `DataSource.get_hostname(fqdn=...)`, minus `resolve_ip`.
///
/// `resolve_ip` is never true on any path a boot takes — only
/// `cc_update_etc_hosts` passes it — and it would make a DNS lookup out of a
/// metadata value, so it is left out (deviation).
#[must_use]
pub fn datasource_hostname(
    metadata: Option<&Object>,
    fqdn: bool,
    root: &Path,
) -> DatasourceHostname {
    let mut is_default = false;
    let local = metadata
        .and_then(|meta| meta.get("local-hostname"))
        .filter(|value| !is_falsy(value))
        .map(py_str);

    let toks: Vec<String> = match local {
        None => {
            let hostname = system_hostname(root);
            if hostname == DEFAULT_HOST {
                is_default = true;
            }
            let hosts_fqdn = fqdn_from_hosts(&hostname, root);
            if let Some(found) =
                hosts_fqdn.filter(|f| f.find('.').is_some_and(|at| at > 0))
            {
                found.split('.').map(ToOwned::to_owned).collect()
            } else if hostname.find('.').is_some_and(|at| at > 0) {
                hostname.split('.').map(ToOwned::to_owned).collect()
            } else if hostname.is_empty() {
                vec![DEFAULT_HOST.to_owned(), DEFAULT_DOMAIN.to_owned()]
            } else {
                vec![hostname, DEFAULT_DOMAIN.to_owned()]
            }
        }
        // An IPv4 address in `local-hostname` becomes `ip-a-b-c-d` (LP #475354).
        Some(local) if is_ipv4(&local) => {
            vec![format!("ip-{}", local.replace('.', "-"))]
        }
        Some(local) => local.split('.').map(ToOwned::to_owned).collect(),
    };

    let hostname = toks.first().cloned().unwrap_or_default();
    let domain = match toks.get(1..) {
        Some(rest) if !rest.is_empty() => rest.join("."),
        _ => DEFAULT_DOMAIN.to_owned(),
    };

    DatasourceHostname {
        hostname: if fqdn && domain != DEFAULT_DOMAIN {
            format!("{hostname}.{domain}")
        } else {
            hostname
        },
        is_default,
    }
}

/// `util.get_hostname()`, which is `socket.gethostname()`.
///
/// Read from `/proc/sys/kernel/hostname` rather than through libc: it is the
/// same value the syscall returns, and this crate forbids `unsafe`.
#[must_use]
pub fn system_hostname(root: &Path) -> String {
    std::fs::read_to_string(root.join("proc/sys/kernel/hostname"))
        .map(|text| text.trim_end_matches('\n').to_owned())
        .unwrap_or_default()
}

/// `util.get_fqdn_from_hosts(hostname)`.
///
/// The canonical name of the first `/etc/hosts` line that lists `hostname` as
/// an *alias* — the third field onward. A line with fewer than three fields
/// cannot have one and is skipped.
#[must_use]
pub fn fqdn_from_hosts(hostname: &str, root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("etc/hosts")).ok()?;
    for line in crate::pystr::split_lines(&text) {
        let line = match line.find('#') {
            Some(at) => line.get(..at).unwrap_or(""),
            None => line,
        };
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 3 {
            continue;
        }
        if toks
            .get(2..)
            .is_some_and(|aliases| aliases.contains(&hostname))
        {
            return toks.get(1).map(|&canonical| canonical.to_owned());
        }
    }
    None
}

/// `socket.getfqdn()`, as far as it can be reached without a resolver.
///
/// `CPython` takes the system hostname and asks `gethostbyaddr` for the first
/// alias containing a dot. This crate forbids `unsafe` and has no resolver, so
/// it consults `/etc/hosts` -- which is where the answer comes from on a
/// freshly-booted instance -- and otherwise returns the hostname itself, which
/// is also what `socket.getfqdn` falls back to (deviation 164).
#[must_use]
pub fn getfqdn(root: &Path) -> String {
    let hostname = system_hostname(root);
    if hostname.is_empty() {
        return "localhost".to_owned();
    }
    if hostname.contains('.') {
        return hostname;
    }
    canonical_from_hosts(&hostname, root).unwrap_or(hostname)
}

/// The canonical name of the first `/etc/hosts` line naming `hostname`, alias
/// or canonical -- which is the lookup `gethostbyaddr` answers from.
fn canonical_from_hosts(hostname: &str, root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("etc/hosts")).ok()?;
    for line in crate::pystr::split_lines(&text) {
        let line = match line.find('#') {
            Some(at) => line.get(..at).unwrap_or(""),
            None => line,
        };
        let toks: Vec<&str> = line.split_whitespace().collect();
        if !toks.get(1..).is_some_and(|names| names.contains(&hostname)) {
            continue;
        }
        if let Some(canonical) = toks
            .get(1..)
            .and_then(|names| names.iter().find(|name| name.contains('.')).copied())
        {
            return Some(canonical.to_owned());
        }
    }
    None
}

/// `net.is_ipv4_address`.
fn is_ipv4(text: &str) -> bool {
    text.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Python truthiness for the values `local-hostname` can hold.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(n) => n.as_f64() == Some(0.0),
    }
}

fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => ci_config::repr::repr(other),
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

    fn fixture(hostname: &str, hosts: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("proc/sys/kernel")).unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("proc/sys/kernel/hostname"), hostname).unwrap();
        std::fs::write(dir.path().join("etc/hosts"), hosts).unwrap();
        dir
    }

    fn cfg(pairs: &[(&str, &str)]) -> Object {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::from(*v)))
            .collect()
    }

    fn meta(local: &str) -> Object {
        cfg(&[("local-hostname", local)])
    }

    #[test]
    fn an_fqdn_in_the_config_supplies_the_short_name_too() {
        let root = fixture("ignored", "");
        let got =
            get_hostname_fqdn(&cfg(&[("fqdn", "a.example.com")]), None, root.path());
        assert_eq!(got.hostname, "a");
        assert_eq!(got.fqdn, "a.example.com");
        assert!(!got.is_default);
    }

    #[test]
    fn an_explicit_hostname_beats_the_one_derived_from_the_fqdn() {
        let root = fixture("ignored", "");
        let got = get_hostname_fqdn(
            &cfg(&[("fqdn", "a.example.com"), ("hostname", "b")]),
            None,
            root.path(),
        );
        assert_eq!(
            (got.hostname.as_str(), got.fqdn.as_str()),
            ("b", "a.example.com")
        );
    }

    #[test]
    fn a_dotted_hostname_is_taken_as_both_names() {
        let root = fixture("ignored", "");
        let got = get_hostname_fqdn(
            &cfg(&[("hostname", "a.example.com")]),
            None,
            root.path(),
        );
        assert_eq!(
            (got.hostname.as_str(), got.fqdn.as_str()),
            ("a", "a.example.com")
        );
    }

    #[test]
    fn metadata_supplies_both_names_when_the_config_is_silent() {
        let root = fixture("ignored", "");
        let got = get_hostname_fqdn(
            &Object::new(),
            Some(&meta("host1.example.com")),
            root.path(),
        );
        assert_eq!(
            (got.hostname.as_str(), got.fqdn.as_str()),
            ("host1", "host1.example.com")
        );
    }

    #[test]
    fn a_bare_metadata_name_gets_no_domain_in_the_fqdn() {
        let root = fixture("ignored", "");
        let got = get_hostname_fqdn(&Object::new(), Some(&meta("host1")), root.path());
        assert_eq!(
            (got.hostname.as_str(), got.fqdn.as_str()),
            ("host1", "host1"),
            "the domain is `localdomain`, which the fqdn branch drops"
        );
    }

    #[test]
    fn an_ipv4_local_hostname_becomes_an_ip_prefixed_name() {
        let root = fixture("ignored", "");
        let got =
            get_hostname_fqdn(&Object::new(), Some(&meta("10.0.0.4")), root.path());
        assert_eq!(got.hostname, "ip-10-0-0-4");
    }

    #[test]
    fn with_nothing_at_all_the_system_name_is_used_and_localhost_is_flagged() {
        let root = fixture("localhost", "");
        let got = get_hostname_fqdn(&Object::new(), None, root.path());
        assert_eq!(got.hostname, "localhost");
        assert!(got.is_default, "nothing asked for localhost; the image did");

        let root = fixture("chosen", "");
        let got = get_hostname_fqdn(&Object::new(), None, root.path());
        assert_eq!(got.hostname, "chosen");
        assert!(!got.is_default);
    }

    #[test]
    fn etc_hosts_can_supply_the_domain_for_a_bare_system_name() {
        let root = fixture("host1", "127.0.1.1 host1.example.com host1\n");
        let got = get_hostname_fqdn(&Object::new(), None, root.path());
        assert_eq!(
            (got.hostname.as_str(), got.fqdn.as_str()),
            ("host1", "host1.example.com")
        );
    }

    #[test]
    fn a_hosts_line_with_no_alias_column_is_ignored() {
        let root = fixture("host1", "# comment\n127.0.1.1 host1\n");
        assert_eq!(fqdn_from_hosts("host1", root.path()), None);
    }

    #[test]
    fn a_commented_tail_does_not_become_an_alias() {
        let root = fixture("host1", "127.0.1.1 wrong.example.com other # host1\n");
        assert_eq!(fqdn_from_hosts("host1", root.path()), None);
    }

    #[test]
    fn an_empty_local_hostname_falls_through_to_the_system() {
        let root = fixture("host1", "");
        let got = get_hostname_fqdn(&Object::new(), Some(&meta("")), root.path());
        assert_eq!(got.hostname, "host1");
    }
}
