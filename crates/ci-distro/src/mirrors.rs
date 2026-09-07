//! The `package_mirrors` half of `cloudinit/distros/__init__.py`.
//!
//! `system_info.package_mirrors` is how a cloud image points apt at a mirror
//! inside its own network without putting anything in user-data. The Azure
//! images ship a `/etc/cloud/cloud.cfg.d/90-azure.cfg` that does exactly that,
//! so `cc_apt_configure` gets `http://azure.archive.ubuntu.com/ubuntu/` rather
//! than the architecture's public default — which is why this is not optional.
//!
//! The entry for an architecture has two halves: a `failsafe` that is used as
//! written, and a `search` list of templates that are substituted, sanitised
//! and then probed, the first one that resolves winning.

use std::collections::BTreeMap;

use ci_config::{Object, Value};
use ci_log::Logger;

const SOURCE: &str = "distros/__init__.py";

/// `LDH_ASCII_CHARS + "."`, the characters a sanitised hostname may keep.
fn is_acceptable(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '.'
}

/// `_get_arch_package_mirror_info`: the entry for this architecture, else the
/// one that claims `default`, else nothing.
#[must_use]
pub fn arch_package_mirror_info<'a>(
    package_mirrors: &'a [Value],
    arch: &str,
) -> Option<&'a Object> {
    let mut default = None;
    for item in package_mirrors {
        let Some(item) = item.as_object() else {
            continue;
        };
        // `item.get("arches")` with no default: upstream raises `TypeError` on
        // an entry without one. Treating it as empty is the same outcome for
        // every config that does not crash.
        let arches = item.get("arches").and_then(Value::as_array);
        let Some(arches) = arches else { continue };
        let names: Vec<&str> = arches.iter().filter_map(Value::as_str).collect();
        if names.contains(&arch) {
            return Some(item);
        }
        if names.contains(&"default") {
            default = Some(item);
        }
    }
    default
}

/// What went wrong applying a `%`-format template.
enum FormatError {
    /// `KeyError` — the template asked for a substitution that is not set.
    MissingKey,
    /// Anything else `str.__mod__` raises, which upstream does not catch.
    Malformed(String),
}

/// `template % subst` for the `%(name)s` subset these templates use.
fn percent_format(
    template: &str,
    subst: &BTreeMap<&str, String>,
) -> Result<String, FormatError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('%') {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let mut chars = tail.chars();
        chars.next();
        match chars.next() {
            Some('%') => {
                out.push('%');
                rest = chars.as_str();
            }
            Some('(') => {
                let after = chars.as_str();
                let Some(close) = after.find(')') else {
                    return Err(FormatError::Malformed(
                        "incomplete format key".to_owned(),
                    ));
                };
                let (key, after) = after.split_at(close);
                // Past the ')'; the conversion character follows.
                let mut after = after.chars();
                after.next();
                match after.next() {
                    Some('s') => {}
                    Some(other) => {
                        return Err(FormatError::Malformed(format!(
                        "unsupported format character '{other}' (0x{:x}) at index {}",
                        u32::from(other),
                        template.len().saturating_sub(after.as_str().len()),
                    )))
                    }
                    None => {
                        return Err(FormatError::Malformed(
                            "incomplete format".to_owned(),
                        ))
                    }
                }
                let Some(value) = subst.get(key) else {
                    return Err(FormatError::MissingKey);
                };
                out.push_str(value);
                rest = after.as_str();
            }
            Some(other) => {
                return Err(FormatError::Malformed(format!(
                    "unsupported format character '{other}' (0x{:x}) at index {at}",
                    u32::from(other),
                )))
            }
            None => return Err(FormatError::Malformed("incomplete format".to_owned())),
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A URL split the way `urllib.parse.urlsplit` splits it, keeping only what
/// `_apply_hostname_transformations_to_url` puts back together.
struct Split<'a> {
    scheme: &'a str,
    /// Lowercased and stripped of userinfo and port, as `parts.hostname` is.
    hostname: String,
    port: Option<&'a str>,
    /// Path, query and fragment, still joined.
    tail: &'a str,
}

fn urlsplit(url: &str) -> Option<Split<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // `parts.hostname` keeps an IPv6 literal without its brackets.
    let (host, port) = match host.strip_prefix('[') {
        Some(inner) => {
            let (addr, after) = inner.split_once(']')?;
            (addr, after.strip_prefix(':'))
        }
        None => match host.split_once(':') {
            Some((addr, port)) => (addr, Some(port)),
            None => (host, None),
        },
    };
    Some(Split {
        scheme,
        hostname: host.to_lowercase(),
        port: port.filter(|port| !port.is_empty()),
        tail,
    })
}

/// `_sanitize_mirror_url`: make the hostname of a substituted template into
/// something that is at least a legal domain name.
///
/// Returns `None` for a URL with no hostname to work on, which is upstream's
/// signal to drop the candidate.
#[must_use]
pub fn sanitize_mirror_url(url: &str) -> Option<String> {
    let parts = urlsplit(url)?;
    if parts.hostname.is_empty() {
        return None;
    }
    // An IP address is not a domain name, so none of the transformations
    // apply and the URL is returned exactly as it came in.
    if parts.hostname.parse::<std::net::IpAddr>().is_ok() {
        return Some(url.to_owned());
    }
    // Upstream first converts the hostname to its IDNA A-label form. A
    // hostname that is already ASCII passes through that unchanged, and one
    // that is not needs Punycode and IDNA2003 nameprep — neither of which is
    // implemented here, so such a URL is returned untouched rather than
    // mangled. See docs/COMPAT.md.
    if !parts.hostname.is_ascii() {
        return Some(url.to_owned());
    }
    let hostname: String = parts
        .hostname
        .chars()
        .map(|ch| if is_acceptable(ch) { ch } else { '-' })
        .collect();
    let hostname = hostname
        .split('.')
        .map(|label| label.trim_matches('-'))
        .collect::<Vec<&str>>()
        .join(".");
    let netloc = match parts.port {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname,
    };
    Some(format!("{}://{netloc}{}", parts.scheme, parts.tail))
}

/// `re.match("^[a-z][a-z]-(?:[a-z]+-)+[0-9][a-z]$", zone)`, upstream's guess at
/// whether an availability zone is an EC2 one.
fn looks_like_ec2_zone(zone: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            #[allow(clippy::unwrap_used)]
            regex::Regex::new("^[a-z][a-z]-(?:[a-z]+-)+[0-9][a-z]$").unwrap()
        })
        .is_match(zone)
}

/// `_get_package_mirror_info`: reduce one architecture's entry to a mapping of
/// `{name: mirror}`.
///
/// # Errors
///
/// A template that is not a legal `%`-format string; upstream catches only the
/// `KeyError` of an unset substitution.
pub fn package_mirror_info(
    mirror_info: Option<&Object>,
    availability_zone: Option<&str>,
    region: Option<&str>,
    platform_type: &str,
    search: &mut dyn FnMut(&[String], &mut Logger) -> Option<String>,
    log: &mut Logger,
) -> Result<Object, String> {
    let mut subst: BTreeMap<&str, String> = BTreeMap::new();
    if let Some(zone) = availability_zone.filter(|zone| !zone.is_empty()) {
        subst.insert("availability_zone", zone.to_owned());
        // EC2 zones are named `<region><letter>`, so the region is the zone
        // without its last character. Upstream calls this a best guess.
        if looks_like_ec2_zone(zone) {
            let ec2_region = zone.get(..zone.len().saturating_sub(1)).unwrap_or("");
            if ci_core::features::ALLOW_EC2_MIRRORS_ON_NON_AWS_INSTANCE_TYPES
                || platform_type == "ec2"
            {
                subst.insert("ec2_region", ec2_region.to_owned());
            }
        }
    }
    if let Some(region) = region.filter(|region| !region.is_empty()) {
        subst.insert("region", region.to_owned());
    }

    let mut results = Object::new();
    let empty = Object::new();
    let mirror_info = mirror_info.unwrap_or(&empty);
    if let Some(failsafe) = mirror_info.get("failsafe").and_then(Value::as_object) {
        for (name, mirror) in failsafe {
            results.insert(name.clone(), mirror.clone());
        }
    }
    if let Some(searches) = mirror_info.get("search").and_then(Value::as_object) {
        for (name, searchlist) in searches {
            let mut mirrors: Vec<String> = Vec::new();
            for tmpl in searchlist.as_array().unwrap_or(&Vec::new()) {
                let Some(tmpl) = tmpl.as_str() else { continue };
                let mirror = match percent_format(tmpl, &subst) {
                    Ok(mirror) => mirror,
                    Err(FormatError::MissingKey) => continue,
                    Err(FormatError::Malformed(message)) => return Err(message),
                };
                if let Some(mirror) = sanitize_mirror_url(&mirror) {
                    mirrors.push(mirror);
                }
            }
            if let Some(found) = search(&mirrors, log) {
                results.insert(name.clone(), Value::String(found));
            }
        }
    }
    log.debug(
        SOURCE,
        &format!(
            "filtered distro mirror info: {}",
            ci_config::repr(&Value::Object(results.clone()))
        ),
    );
    Ok(results)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn value(text: &str) -> Value {
        ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default()).unwrap()
    }

    #[test]
    fn the_arch_entry_wins_over_the_default_entry_wherever_it_sits() {
        let Value::Array(items) = value(
            "- arches: [default]\n  failsafe: {primary: d}\n\
             - arches: [arm64]\n  failsafe: {primary: a}\n",
        ) else {
            panic!("not a list")
        };
        assert_eq!(
            arch_package_mirror_info(&items, "arm64")
                .and_then(|item| item.get("failsafe")),
            Some(&value("{primary: a}"))
        );
        assert_eq!(
            arch_package_mirror_info(&items, "s390x")
                .and_then(|item| item.get("failsafe")),
            Some(&value("{primary: d}"))
        );
    }

    #[test]
    fn a_template_needing_a_substitution_nobody_set_is_skipped_not_fatal() {
        // The Azure images' own entry: one search URL with nothing to
        // substitute, next to the stock ones that need a zone or a region.
        let info = value(
            "failsafe:\n  primary: http://archive.ubuntu.com/ubuntu\n\
             search:\n  primary:\n\
             \x20   - http://%(availability_zone)s.clouds.archive.ubuntu.com/ubuntu/\n\
             \x20   - http://azure.archive.ubuntu.com/ubuntu/\n",
        );
        let mut log = Logger::silent();
        let mut seen: Vec<String> = Vec::new();
        let found = package_mirror_info(
            info.as_object(),
            None,
            None,
            "azure",
            &mut |candidates, _| {
                seen.extend_from_slice(candidates);
                candidates.first().cloned()
            },
            &mut log,
        )
        .unwrap();
        assert_eq!(seen, ["http://azure.archive.ubuntu.com/ubuntu/"]);
        assert_eq!(
            found.get("primary"),
            Some(&Value::String(
                "http://azure.archive.ubuntu.com/ubuntu/".to_owned()
            ))
        );
    }

    #[test]
    fn a_search_that_finds_nothing_leaves_the_failsafe_standing() {
        let info = value(
            "failsafe: {primary: http://ports.ubuntu.com/ubuntu-ports}\n\
             search: {primary: [http://nowhere.invalid/ubuntu/]}\n",
        );
        let mut log = Logger::silent();
        let found = package_mirror_info(
            info.as_object(),
            None,
            None,
            "azure",
            &mut |_, _| None,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            found.get("primary"),
            Some(&Value::String(
                "http://ports.ubuntu.com/ubuntu-ports".to_owned()
            ))
        );
    }

    #[test]
    fn only_an_ec2_zone_on_an_ec2_platform_yields_an_ec2_region() {
        let info = value(
            "search:\n  primary:\n\
             \x20   - http://%(ec2_region)s.ec2.archive.ubuntu.com/ubuntu/\n",
        );
        let mut log = Logger::silent();
        let mut probe =
            |candidates: &[String], _: &mut Logger| candidates.first().cloned();
        let on = package_mirror_info(
            info.as_object(),
            Some("us-east-1b"),
            None,
            "ec2",
            &mut probe,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            on.get("primary"),
            Some(&Value::String(
                "http://us-east-1.ec2.archive.ubuntu.com/ubuntu/".to_owned()
            ))
        );
        // The same zone on another platform leaves `ec2_region` unset, so the
        // only candidate is skipped and nothing is chosen.
        let off = package_mirror_info(
            info.as_object(),
            Some("us-east-1b"),
            None,
            "azure",
            &mut probe,
            &mut log,
        )
        .unwrap();
        assert_eq!(off.get("primary"), None);
        // A zone that is not shaped like EC2's does not become a region either.
        let odd = package_mirror_info(
            info.as_object(),
            Some("westus3"),
            None,
            "ec2",
            &mut probe,
            &mut log,
        )
        .unwrap();
        assert_eq!(odd.get("primary"), None);
    }

    #[test]
    fn a_substituted_hostname_is_reduced_to_letters_digits_and_hyphens() {
        assert_eq!(
            sanitize_mirror_url("http://us east_1.archive.example/ubuntu/"),
            Some("http://us-east-1.archive.example/ubuntu/".to_owned())
        );
        // Leading and trailing hyphens go, per label, after the replacement.
        assert_eq!(
            sanitize_mirror_url("http://_us_.archive.example/"),
            Some("http://us.archive.example/".to_owned())
        );
        // Userinfo is dropped, because upstream rebuilds the netloc from the
        // hostname alone; the port survives.
        assert_eq!(
            sanitize_mirror_url("http://user:pw@Host.Example:8080/x?y#z"),
            Some("http://host.example:8080/x?y#z".to_owned())
        );
        // An IP address is not a domain name and is returned untouched, case
        // and userinfo included.
        assert_eq!(
            sanitize_mirror_url("http://User@192.168.0.1:80/ubuntu/"),
            Some("http://User@192.168.0.1:80/ubuntu/".to_owned())
        );
        assert_eq!(sanitize_mirror_url("not a url"), None);
    }
}
