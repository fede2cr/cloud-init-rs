//! `cc_apt_configure`: point apt at the right mirrors, sources and keys.
//!
//! The module is upstream's largest, and splits cleanly in two. This file
//! holds the half that only *decides*: converting the three historical config
//! shapes into one, picking a mirror for the architecture, and rewriting a
//! sources template to disable suites. None of it touches the system, so all
//! of it is directly testable and is what the differential dumps compare.
//!
//! The half that acts — importing keys, writing `sources.list.d` entries and
//! reconfiguring debconf — lives alongside it and is driven from [`handle`].

use ci_config::{option, Object, Value};
use ci_gpg::Gpg;
use ci_log::Logger;

use super::Args;

/// `cc_apt_configure.py:30`. Matches `ppa:foo` and `cloud-archive:bar`.
pub const ADD_APT_REPO_MATCH: &str = r"^[\w-]+:\w";

pub const APT_LOCAL_KEYS: &str = "/etc/apt/trusted.gpg";
pub const APT_TRUSTED_GPG_DIR: &str = "/etc/apt/trusted.gpg.d/";
pub const CLOUD_INIT_GPG_DIR: &str = "/etc/apt/cloud-init.gpg.d/";
pub const DISABLE_SUITES_REDACT_PREFIX: &str = "# cloud-init disable_suites redacted: ";

/// Where apt caches repository indexes.
pub const APT_LISTS: &str = "/var/lib/apt/lists";

pub const APT_CONFIG_FN: &str = "/etc/apt/apt.conf.d/94cloud-init-config";
pub const APT_PROXY_FN: &str = "/etc/apt/apt.conf.d/90cloud-init-aptproxy";

pub const PRIMARY_ARCH_MIRRORS: [(&str, &str); 2] = [
    ("PRIMARY", "http://archive.ubuntu.com/ubuntu/"),
    ("SECURITY", "http://security.ubuntu.com/ubuntu/"),
];
pub const PORTS_MIRRORS: [(&str, &str); 2] = [
    ("PRIMARY", "http://ports.ubuntu.com/ubuntu-ports"),
    ("SECURITY", "http://ports.ubuntu.com/ubuntu-ports"),
];
pub const PRIMARY_ARCHES: [&str; 2] = ["amd64", "i386"];
pub const PORTS_ARCHES: [&str; 6] =
    ["s390x", "arm64", "armhf", "powerpc", "ppc64el", "riscv64"];

pub const UBUNTU_DEFAULT_APT_SOURCES_LIST: &str = "\
# Ubuntu sources have moved to the /etc/apt/sources.list.d/ubuntu.sources
# file, which uses the deb822 format. Use deb822-formatted .sources files
# to manage package sources in the /etc/apt/sources.list.d/ directory.
# See the sources.list(5) manual page for details.
";

const SOURCE: &str = "cc_apt_configure.py";

/// `get_default_mirrors`: the archive depends on whether the arch is a port.
///
/// # Errors
///
/// An arch in neither list, which upstream raises `ValueError` for.
pub fn get_default_mirrors(arch: &str) -> Result<Vec<(String, String)>, String> {
    let table = if PRIMARY_ARCHES.contains(&arch) {
        PRIMARY_ARCH_MIRRORS
    } else if PORTS_ARCHES.contains(&arch) {
        PORTS_MIRRORS
    } else {
        return Err(format!("No default mirror known for arch {arch}"));
    };
    Ok(table
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect())
}

// ---------------------------------------------------------------------------
// v1 -> v2 -> v3 config conversion
// ---------------------------------------------------------------------------

/// `convert_key`: move `oldkey` to `newkey` if it is present and not null.
fn convert_key(
    oldcfg: &mut Object,
    aptcfg: &mut Object,
    oldkey: &str,
    newkey: &str,
) -> bool {
    // `if oldcfg.get(oldkey, None) is not None` — an explicit null does not move.
    let Some(value) = oldcfg.get(oldkey) else {
        return false;
    };
    if value.is_null() {
        return false;
    }
    let value = value.clone();
    aptcfg.insert(newkey.to_owned(), value);
    oldcfg.remove(oldkey);
    true
}

/// `convert_mirror`: fold the flat `apt_mirror*` keys into one `primary` entry.
fn convert_mirror(oldcfg: &mut Object, aptcfg: &mut Object) {
    const KEYMAP: [(&str, &str); 3] = [
        ("apt_mirror", "uri"),
        ("apt_mirror_search", "search"),
        ("apt_mirror_search_dns", "search_dns"),
    ];
    let mut newmcfg = Object::new();
    newmcfg.insert(
        "arches".to_owned(),
        Value::Array(vec![Value::String("default".to_owned())]),
    );
    let mut converted = false;
    for (oldkey, newkey) in KEYMAP {
        if convert_key(oldcfg, &mut newmcfg, oldkey, newkey) {
            converted = true;
        }
    }
    if converted {
        aptcfg.insert(
            "primary".to_owned(),
            Value::Array(vec![Value::Object(newmcfg)]),
        );
    }
}

/// The v2 key names, and where each lands in v3. `None` means "drop it".
///
/// Note the third and fourth rows: upstream maps `apt_ftp_proxy` onto
/// `https_proxy` and `apt_https_proxy` onto `ftp_proxy`. The two are
/// transposed, and the port reproduces it. See COMPAT.md bug B76.
const MAP_OLD_KEYS: [(&str, Option<&str>); 11] = [
    ("apt_sources", Some("sources")),
    ("apt_mirror", None),
    ("apt_mirror_search", None),
    ("apt_mirror_search_dns", None),
    ("apt_proxy", Some("proxy")),
    ("apt_http_proxy", Some("http_proxy")),
    ("apt_ftp_proxy", Some("https_proxy")),
    ("apt_https_proxy", Some("ftp_proxy")),
    ("apt_preserve_sources_list", Some("preserve_sources_list")),
    ("apt_custom_sources_list", Some("sources_list")),
    ("add_apt_repo_match", Some("add_apt_repo_match")),
];

/// `convert_v1_to_v2_apt_format`: `apt_sources` as a list becomes a dict.
///
/// # Errors
///
/// Anything that is neither a list nor a dict.
pub fn convert_v1_to_v2_apt_format(
    srclist: &Value,
    log: &mut Logger,
    mut unique_key: impl FnMut(&Object, &str) -> String,
) -> Result<Object, String> {
    log.debug(
        SOURCE,
        "Config key 'apt_sources' is deprecated in 22.1. Use 'apt' instead",
    );
    match srclist {
        Value::Array(items) => {
            log.debug(
                SOURCE,
                "apt config: convert V1 to V2 format (source list to dict)",
            );
            let mut srcdict = Object::new();
            for item in items {
                let mut entry = item.as_object().cloned().unwrap_or_default();
                let key = match entry.get("filename").and_then(Value::as_str) {
                    // All the unnamed entries would collide on one key, so
                    // upstream keeps the colliding filename but files them
                    // under randomised keys so that none is lost.
                    None => {
                        entry.insert(
                            "filename".to_owned(),
                            Value::String("cloud_config_sources.list".to_owned()),
                        );
                        unique_key(&srcdict, "cloud_config_sources.list")
                    }
                    Some(filename) => filename.to_owned(),
                };
                srcdict.insert(key, Value::Object(entry));
            }
            Ok(srcdict)
        }
        Value::Object(map) => Ok(map.clone()),
        _ => Err("unknown apt_sources format".to_owned()),
    }
}

/// `convert_v2_to_v3_apt_format`: move the flat keys under `apt`.
///
/// # Errors
///
/// A key defined in both the old and the new shape with different values, and
/// an old key that survived conversion.
pub fn convert_v2_to_v3_apt_format(
    oldcfg: &mut Object,
    log: &mut Logger,
) -> Result<(), String> {
    let mut needtoconvert: Vec<&str> = Vec::new();
    for (oldkey, _) in MAP_OLD_KEYS {
        let Some(value) = oldcfg.get(oldkey) else {
            continue;
        };
        // `if oldcfg[oldkey] in (None, "")` — null and empty string are dropped.
        if value.is_null() || value.as_str() == Some("") {
            oldcfg.remove(oldkey);
        } else {
            needtoconvert.push(oldkey);
        }
    }
    if needtoconvert.is_empty() {
        return Ok(());
    }
    log.debug(
        SOURCE,
        &format!(
            "The following config key(s): {needtoconvert:?} are deprecated in 22.1"
        ),
    );

    // LP #1616831: when both shapes are present the new one wins, but a
    // disagreement between them is an error rather than a silent choice.
    if let Some(newaptcfg) = oldcfg.get("apt").and_then(Value::as_object).cloned() {
        log.debug(
            SOURCE,
            "Support for combined old and new apt module keys is deprecated in 22.1",
        );
        for oldkey in &needtoconvert {
            let newkey = MAP_OLD_KEYS
                .iter()
                .find(|(key, _)| key == oldkey)
                .and_then(|(_, newkey)| *newkey);
            let verify = oldcfg.remove(*oldkey);
            let Some(newkey) = newkey else { continue };
            let Some(existing) = newaptcfg.get(newkey) else {
                continue;
            };
            if existing.is_null() {
                continue;
            }
            if verify.as_ref() != Some(existing) {
                return Err(format!(
                    "Old and New apt format defined with unequal values {} vs {} @ {oldkey}",
                    verify.map_or_else(|| "None".to_owned(), |v| ci_config::repr(&v)),
                    ci_config::repr(existing),
                ));
            }
        }
        return Ok(());
    }

    let mut aptcfg = Object::new();
    for (oldkey, newkey) in MAP_OLD_KEYS {
        if let Some(newkey) = newkey {
            convert_key(oldcfg, &mut aptcfg, oldkey, newkey);
        }
    }
    convert_mirror(oldcfg, &mut aptcfg);

    for (oldkey, _) in MAP_OLD_KEYS {
        if oldcfg.get(oldkey).is_some_and(|value| !value.is_null()) {
            return Err(format!("old apt key '{oldkey}' left after conversion"));
        }
    }
    oldcfg.insert("apt".to_owned(), Value::Object(aptcfg));
    Ok(())
}

/// `convert_to_v3_apt_format`: run both conversions in order.
///
/// # Errors
///
/// Whatever either conversion rejects.
pub fn convert_to_v3_apt_format(
    cfg: &mut Object,
    log: &mut Logger,
    unique_key: impl FnMut(&Object, &str) -> String,
) -> Result<(), String> {
    if let Some(apt_sources) = cfg.get("apt_sources") {
        if !apt_sources.is_null() {
            let converted =
                convert_v1_to_v2_apt_format(&apt_sources.clone(), log, unique_key)?;
            cfg.insert("apt_sources".to_owned(), Value::Object(converted));
        }
    }
    convert_v2_to_v3_apt_format(cfg, log)
}

// ---------------------------------------------------------------------------
// mirror selection
// ---------------------------------------------------------------------------

/// `get_arch_mirrorconfig`: the entry matching `arch`, else the `default` one.
pub fn get_arch_mirrorconfig<'a>(
    cfg: &'a Object,
    mirrortype: &str,
    arch: &str,
) -> Option<&'a Object> {
    let list = cfg.get(mirrortype)?.as_array()?;
    let mut default = None;
    for element in list {
        let Some(entry) = element.as_object() else {
            continue;
        };
        let arches: Vec<&str> = entry
            .get("arches")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if arches.contains(&arch) {
            return Some(entry);
        }
        if arches.contains(&"default") {
            default = Some(entry);
        }
    }
    default
}

/// `get_mirror`: a direct `uri`, else a `search` probe, else a `search_dns` one.
///
/// The two probes take the logger as an argument rather than capturing it,
/// because upstream's `search_for_mirror` logs from inside them and the caller
/// logs between them; threading it is the only way to keep both.
pub fn get_mirror(
    cfg: &Object,
    mirrortype: &str,
    arch: &str,
    search: &mut dyn FnMut(&[String], &mut Logger) -> Option<String>,
    search_dns: &mut dyn FnMut(&Value, &str, &mut Logger) -> Option<String>,
    log: &mut Logger,
) -> Option<String> {
    let mcfg = get_arch_mirrorconfig(cfg, mirrortype, arch)?;
    if let Some(uri) = mcfg.get("uri").and_then(Value::as_str) {
        return Some(uri.to_owned());
    }
    // `mcfg.get("search", None)`: an absent key never reaches
    // `search_for_mirror`, so it does not get the log line an empty list does.
    let candidates = mcfg
        .get("search")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        });
    if let Some(candidates) = candidates {
        if let Some(found) = search(&candidates, log) {
            return Some(found);
        }
    }
    let configured = mcfg.get("search_dns").cloned().unwrap_or(Value::Null);
    search_dns(&configured, mirrortype, log)
}

/// `search_for_mirror_dns`: the `<distro>-mirror.<domain>` naming convention.
///
/// Returns the candidate hostnames to probe, in upstream's order. Doing the
/// probing is the caller's job, because it is the only part that needs a
/// network.
pub fn mirror_dns_candidates(
    mirrortype: &str,
    fqdn: &str,
    distro: &str,
) -> Result<Vec<String>, String> {
    let mirrordns = match mirrortype {
        "primary" => "mirror",
        "security" => "security-mirror",
        _ => return Err("unknown mirror type".to_owned()),
    };
    let mut doms: Vec<String> = Vec::new();
    // The domain part of the fqdn, i.e. everything after the first label.
    let mydom = fqdn.split('.').skip(1).collect::<Vec<&str>>().join(".");
    if !mydom.is_empty() {
        doms.push(format!(".{mydom}"));
    }
    doms.push(".localdomain".to_owned());
    doms.push(String::new());
    Ok(doms
        .iter()
        .map(|post| format!("http://{distro}-{mirrordns}{post}/{distro}"))
        .collect())
}

/// `update_mirror_info`: fill in security from primary, else fall back.
///
/// # Errors
///
/// An architecture with no default mirror, or a datasource mirror mapping
/// missing `primary` or `security` — upstream indexes both without checking,
/// which is bug B78.
pub fn update_mirror_info(
    pmirror: Option<&str>,
    smirror: Option<&str>,
    arch: &str,
    datasource_mirrors: Option<&Object>,
) -> Result<Object, String> {
    let mut out = Object::new();
    if let Some(pmirror) = pmirror {
        let smirror = smirror.unwrap_or(pmirror);
        out.insert("PRIMARY".to_owned(), Value::String(pmirror.to_owned()));
        out.insert("SECURITY".to_owned(), Value::String(smirror.to_owned()));
        return Ok(out);
    }
    if let Some(info) = datasource_mirrors {
        if !info.is_empty() {
            let mut m = info.clone();
            // The datasource speaks lowercase; the templates want uppercase.
            // `m["primary"]` and `m["security"]` are plain lookups upstream, so
            // a `package_mirrors` entry that names only one of them is a
            // `KeyError` rather than a partial answer (B78).
            for key in ["primary", "security"] {
                let Some(value) = info.get(key) else {
                    return Err(format!("'{key}'"));
                };
                m.insert(key.to_uppercase(), value.clone());
            }
            return Ok(m);
        }
    }
    for (key, value) in get_default_mirrors(arch)? {
        out.insert(key, Value::String(value));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// suites
// ---------------------------------------------------------------------------

/// `map_known_suites`: the five shorthands that expand to `$RELEASE`-forms.
pub fn map_known_suites(suite: &str) -> &str {
    match suite {
        "updates" => "$RELEASE-updates",
        "backports" => "$RELEASE-backports",
        "security" => "$RELEASE-security",
        "proposed" => "$RELEASE-proposed",
        "release" => "$RELEASE",
        other => other,
    }
}

/// `is_deb822_sources_format`: which of the two sources syntaxes this is.
pub fn is_deb822_sources_format(content: &str, log: &mut Logger) -> bool {
    let has_one_line = content
        .lines()
        .any(|line| line.starts_with("deb ") || line.starts_with("deb-src "));
    if has_one_line {
        return false;
    }
    let has_deb822 = content.lines().any(|line| {
        line.starts_with("Types: ")
            || line.starts_with("Suites: ")
            || line.starts_with("Components: ")
            || line.starts_with("URIs: ")
    });
    if has_deb822 {
        return true;
    }
    log.warning(
        SOURCE,
        "apt.sources_list value does not match either deb822 source keys or \
         deb/deb-src list keys. Assuming APT deb/deb-src list format.",
    );
    false
}

/// `templater.render_string(text, {"RELEASE": release})`.
fn render_release(text: &str, release: &str) -> Result<String, String> {
    let mut params = Object::new();
    params.insert("RELEASE".to_owned(), Value::String(release.to_owned()));
    ci_template::render_string(text, &Value::Object(params))
        .map_err(|err| err.to_string())
}

/// `textwrap.indent`: prefix every line that is not entirely whitespace.
fn indent_nonempty(text: &str, prefix: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_owned()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect()
}

/// `re.findall(r"\nSuites:[ \t]+([\w-]+)", entry)` as a yes/no.
///
/// The pattern is anchored on a newline, so an entry whose *first* line is
/// `Suites:` reads as having none and gets disabled.
fn has_active_suites(entry: &str) -> bool {
    entry.split('\n').skip(1).any(|line| {
        let Some(rest) = line.strip_prefix("Suites:") else {
            return false;
        };
        let trimmed = rest.trim_start_matches([' ', '\t']);
        // `[ \t]+` needs at least one space, `[\w-]+` at least one word char.
        rest.len() != trimmed.len()
            && trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-')
    })
}

/// `re.sub(r"\nSuites:.*", "", entry)`: drop the newline and the line with it.
fn strip_suites_lines(entry: &str) -> String {
    let mut out = String::new();
    for (index, part) in entry.split('\n').enumerate() {
        if index == 0 {
            out.push_str(part);
            continue;
        }
        if part.starts_with("Suites:") {
            continue;
        }
        out.push('\n');
        out.push_str(part);
    }
    out
}

/// `disable_deb822_section_without_suites`: comment out a suite-less entry.
pub fn disable_deb822_section_without_suites(entry: &str) -> String {
    if has_active_suites(entry) {
        return entry.to_owned();
    }
    let stripped = strip_suites_lines(entry).replace(DISABLE_SUITES_REDACT_PREFIX, "");
    format!(
        "## Entry disabled by cloud-init, due to disable_suites\n{}",
        indent_nonempty(&stripped, "# disabled by cloud-init: ")
    )
}

/// `disable_suites_deb822`: redact disabled suites from each deb822 stanza.
///
/// # Errors
///
/// A suite name that will not render as a template.
pub fn disable_suites_deb822(
    disabled: &[String],
    src: &str,
    release: &str,
    log: &mut Logger,
) -> Result<String, String> {
    let mut disabled_suite_names: Vec<String> = Vec::new();
    for suite in disabled {
        disabled_suite_names.push(render_release(map_known_suites(suite), release)?);
    }
    log.debug(
        SOURCE,
        &format!("Disabling suites {disabled:?} as {disabled_suite_names:?}"),
    );

    let mut new_src: Vec<String> = Vec::new();
    let mut entry = String::new();
    // `splitlines()`, which unlike `split('\n')` yields no trailing empty piece.
    for line in src.lines() {
        if line.starts_with('#') {
            // A comment inside a stanza belongs to it; one outside stands alone.
            if entry.is_empty() {
                new_src.push(line.to_owned());
            } else {
                entry.push_str(line);
                entry.push('\n');
            }
            continue;
        }
        if line.trim().is_empty() {
            if !entry.is_empty() {
                new_src.push(disable_deb822_section_without_suites(&entry));
                entry.clear();
            }
            new_src.push(line.to_owned());
            continue;
        }
        let Some(suites) = line.strip_prefix("Suites:") else {
            entry.push_str(line);
            entry.push('\n');
            continue;
        };
        let mut new_line = line.to_owned();
        if !disabled_suite_names.is_empty() {
            let orig: Vec<&str> = suites.split_whitespace().collect();
            let kept: Vec<&str> = orig
                .iter()
                .filter(|suite| !disabled_suite_names.iter().any(|name| name == *suite))
                .copied()
                .collect();
            if kept != orig {
                entry.push_str(DISABLE_SUITES_REDACT_PREFIX);
                entry.push_str(line);
                entry.push('\n');
                new_line = format!("Suites: {}", kept.join(" "));
            }
        }
        entry.push_str(&new_line);
        entry.push('\n');
    }
    if !entry.is_empty() {
        new_src.push(disable_deb822_section_without_suites(&entry));
    }
    Ok(new_src.join("\n"))
}

/// `disable_suites`: comment out the suites the config asked to drop.
///
/// # Errors
///
/// A sources line with fewer columns than the suite position needs, which
/// upstream raises `IndexError` for. See COMPAT.md bug B77.
pub fn disable_suites(
    disabled: &[String],
    src: &str,
    release: &str,
    log: &mut Logger,
) -> Result<String, String> {
    if disabled.is_empty() {
        return Ok(src.to_owned());
    }
    if is_deb822_sources_format(src, log) {
        return disable_suites_deb822(disabled, src, release, log);
    }

    let mut retsrc = src.to_owned();
    for suite in disabled {
        let suite = map_known_suites(suite);
        let releasesuite = render_release(suite, release)?;
        log.debug(
            SOURCE,
            &format!("Disabling suite {suite} as {releasesuite}"),
        );

        let mut newsrc = String::new();
        // Python splits on more boundaries than `\n`; a sources file holding a
        // form feed would part company here.
        for line in retsrc.split_inclusive('\n') {
            if line.starts_with('#') {
                newsrc.push_str(line);
                continue;
            }
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() > 1 {
                // Options in column 1 may hold spaces, so the suite is at 2 or
                // later: `deb [ arch=amd64 k=v ] http://example.com/debian`.
                let mut pcol = 2;
                if cols.get(1).is_some_and(|col| col.starts_with('[')) {
                    for col in cols.iter().skip(1) {
                        pcol += 1;
                        if col.ends_with(']') {
                            break;
                        }
                    }
                }
                let Some(col) = cols.get(pcol) else {
                    return Err("list index out of range".to_owned());
                };
                if *col == releasesuite {
                    newsrc.push_str("# suite disabled by cloud-init: ");
                }
            }
            newsrc.push_str(line);
        }
        retsrc = newsrc;
    }
    Ok(retsrc)
}

/// Where apt reads its sources from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AptPaths {
    /// `Dir::Etc` + `Dir::Etc::sourcelist`.
    pub sourcelist: String,
    /// `Dir::Etc` + `Dir::Etc::sourceparts`, with a trailing slash.
    pub sourceparts: String,
}

impl Default for AptPaths {
    fn default() -> Self {
        Self {
            sourcelist: "/etc/apt/sources.list".to_owned(),
            sourceparts: "/etc/apt/sources.list.d/".to_owned(),
        }
    }
}

/// `get_apt_cfg`: read the source paths out of `apt-config dump` output.
///
/// `None` stands for an absent or failed `apt-config`, which upstream falls
/// back to `DEFAULT_APT_CFG` for.
pub fn get_apt_cfg(dump: Option<&str>) -> AptPaths {
    let Some(dump) = dump else {
        return AptPaths::default();
    };
    let (mut etc, mut sourcelist, mut sourceparts) =
        ("etc/apt", "sources.list", "sources.list.d");
    for line in dump.lines() {
        let Some((key, rest)) = line.split_once(" \"") else {
            continue;
        };
        let Some(value) = rest.split('"').next() else {
            continue;
        };
        match key.trim() {
            "Dir::Etc" => etc = value,
            "Dir::Etc::sourcelist" => sourcelist = value,
            "Dir::Etc::sourceparts" => sourceparts = value,
            _ => {}
        }
    }
    AptPaths {
        sourcelist: format!("/{etc}/{sourcelist}"),
        sourceparts: format!("/{etc}/{sourceparts}/"),
    }
}

/// A change [`handle`](super::apt_configure) will make to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    Write {
        path: String,
        content: String,
        /// `None` leaves `util.write_file`'s own default alone.
        mode: Option<u32>,
    },
    Remove(String),
}

/// Python's `"%s" %` of a config scalar.
fn py_str(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| ci_config::repr(value), ToOwned::to_owned)
}

/// `util.get_cfg_option_list`-ish: the string items of a config list.
fn string_list(cfg: &Object, key: &str) -> Vec<String> {
    cfg.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(py_str).collect())
        .unwrap_or_default()
}

/// `apply_apt_config`: the proxy and free-form apt.conf drop-ins.
///
/// A config that sets none of them removes any file a previous boot wrote,
/// which is why this needs to know what is already on disk.
pub fn plan_apt_config(
    cfg: &Object,
    proxy_fname: &str,
    config_fname: &str,
    exists: &mut dyn FnMut(&str) -> bool,
    log: &mut Logger,
) -> Vec<FileAction> {
    const PROXIES: [(&str, &str); 4] = [
        ("proxy", "Acquire::http::Proxy"),
        ("http_proxy", "Acquire::http::Proxy"),
        ("ftp_proxy", "Acquire::ftp::Proxy"),
        ("https_proxy", "Acquire::https::Proxy"),
    ];
    let mut actions = Vec::new();
    let proxies: Vec<String> = PROXIES
        .iter()
        .filter_map(|(name, acquire)| {
            let value = cfg.get(*name)?;
            // `if cfg.get(name)` — a falsy value is no proxy at all.
            if !option::py_truthy(value) {
                return None;
            }
            Some(format!("{acquire} \"{}\";", py_str(value)))
        })
        .collect();
    if proxies.is_empty() {
        if exists(proxy_fname) {
            log.debug(
                SOURCE,
                &format!("no apt proxy configured, removed {proxy_fname}"),
            );
            actions.push(FileAction::Remove(proxy_fname.to_owned()));
        }
    } else {
        log.debug(SOURCE, &format!("write apt proxy info to {proxy_fname}"));
        actions.push(FileAction::Write {
            path: proxy_fname.to_owned(),
            content: format!("{}\n", proxies.join("\n")),
            mode: None,
        });
    }

    match cfg.get("conf").filter(|conf| option::py_truthy(conf)) {
        Some(conf) => {
            log.debug(SOURCE, &format!("write apt config info to {config_fname}"));
            actions.push(FileAction::Write {
                path: config_fname.to_owned(),
                content: py_str(conf),
                mode: None,
            });
        }
        None => {
            if exists(config_fname) {
                log.debug(
                    SOURCE,
                    &format!("no apt config configured, removed {config_fname}"),
                );
                actions.push(FileAction::Remove(config_fname.to_owned()));
            }
        }
    }
    actions
}

/// `generate_sources_list`: render the sources template and place it.
///
/// Which file it lands in depends on the rendered *content*, not only on the
/// feature flag: a deb822 body never goes into `sources.list`, and a one-line
/// body never goes into `<distro>.sources`.
///
/// # Errors
///
/// A template that will not render, or a suite line too short to parse.
#[allow(clippy::too_many_arguments)]
pub fn plan_sources_list(
    cfg: &Object,
    release: &str,
    mirrors: &Object,
    distro: &str,
    keys: &Object,
    deb822: bool,
    apt_paths: &AptPaths,
    load_template: &mut dyn FnMut(&str, &mut Logger) -> Option<String>,
    read_file: &mut dyn FnMut(&str) -> Option<String>,
    log: &mut Logger,
) -> Result<Vec<FileAction>, String> {
    let apt_sources_list = apt_paths.sourcelist.clone();
    let apt_sources_deb822 = format!("{}{distro}.sources", apt_paths.sourceparts);
    let mut aptsrc_file = if deb822 {
        apt_sources_deb822.clone()
    } else {
        apt_sources_list.clone()
    };

    let mut params = Object::new();
    params.insert("RELEASE".to_owned(), Value::String(release.to_owned()));
    params.insert("codename".to_owned(), Value::String(release.to_owned()));
    for (key, value) in keys {
        params.insert(key.clone(), value.clone());
    }
    for (key, value) in mirrors {
        params.insert(key.clone(), value.clone());
        params.insert(key.to_lowercase(), value.clone());
    }

    let tmpl = if let Some(value) = cfg
        .get("sources_list")
        .filter(|value| option::py_truthy(value))
    {
        py_str(value)
    } else {
        log.info(SOURCE, "No custom template provided, fall back to builtin");
        let suffix = if deb822 { ".deb822" } else { "" };
        let mut found = load_template(&format!("sources.list.{distro}{suffix}"), log);
        if found.is_none() {
            found = load_template("sources.list", log);
        }
        let Some(content) = found else {
            log.warning(
                SOURCE,
                &format!("No template found, not rendering {aptsrc_file}"),
            );
            return Ok(Vec::new());
        };
        content
    };

    let rendered = ci_template::render_string(&tmpl, &Value::Object(params))
        .map_err(|err| err.to_string())?;
    // `if tmpl:` — an empty template leaves the target file where it started.
    if !tmpl.is_empty() {
        if is_deb822_sources_format(&rendered, log) {
            if aptsrc_file == apt_sources_list {
                log.debug(
                    SOURCE,
                    &format!(
                        "Provided 'sources_list' user-data is deb822 format, writing to {apt_sources_deb822}"
                    ),
                );
                aptsrc_file.clone_from(&apt_sources_deb822);
            }
        } else {
            log.debug(
                SOURCE,
                &format!(
                    "Provided 'sources_list' user-data is not deb822 format, fallback to {apt_sources_list}"
                ),
            );
            aptsrc_file.clone_from(&apt_sources_list);
        }
    }

    let disabled =
        disable_suites(&string_list(cfg, "disable_suites"), &rendered, release, log)?;
    let mut actions = vec![FileAction::Write {
        path: aptsrc_file.clone(),
        content: disabled,
        mode: Some(0o644),
    }];

    if aptsrc_file == apt_sources_deb822 {
        if let Some(existing) = read_file(&apt_sources_list) {
            // Only ubuntu has an allowed stub; every other distro loses the file.
            if distro == "ubuntu" {
                if existing != UBUNTU_DEFAULT_APT_SOURCES_LIST {
                    log.info(
                        SOURCE,
                        &format!("Replacing {apt_sources_list} to favor deb822 source format"),
                    );
                    actions.push(FileAction::Write {
                        path: apt_sources_list,
                        content: UBUNTU_DEFAULT_APT_SOURCES_LIST.to_owned(),
                        mode: None,
                    });
                }
            } else {
                log.info(
                    SOURCE,
                    &format!(
                        "Removing {apt_sources_list} to favor deb822 source format"
                    ),
                );
                actions.push(FileAction::Remove(apt_sources_list));
            }
        }
    }
    Ok(actions)
}

/// A side effect of the key/source pass.
///
/// gpg itself runs during planning — upstream interleaves the fetch with the
/// template expansion, because `$KEY_FILE` is the path the fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A dearmoured key. Binary, so it cannot share [`FileAction`].
    WriteKey {
        path: String,
        content: Vec<u8>,
    },
    WriteSource {
        path: String,
        content: String,
        append: bool,
    },
    AddAptRepository {
        source: String,
    },
    UpdatePackageSources,
}

/// `PACKAGE_DEPENDENCY_BY_COMMAND`.
const PACKAGE_DEPENDENCY_BY_COMMAND: [(&str, &str); 2] = [
    ("add-apt-repository", "software-properties-common"),
    ("gpg", "gnupg"),
];

/// `pathlib.Path(x).stem`.
fn path_stem(value: &str) -> String {
    std::path::Path::new(value)
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
}

/// `apt_key("add", ...)`: dearmour a key and place it.
///
/// Returns the keyring path, or `/dev/null` when it could not be written —
/// which is then interpolated into `$KEY_FILE` exactly as upstream does.
fn apt_key_add(
    gpg: &mut dyn Gpg,
    output_file: &str,
    data: &str,
    hardened: bool,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> String {
    if output_file.is_empty() {
        log.warning(
            SOURCE,
            &format!("Unknown filename, failed to add key: \"{data}\""),
        );
        return "/dev/null".to_owned();
    }
    let key_dir = if hardened {
        CLOUD_INIT_GPG_DIR
    } else {
        APT_TRUSTED_GPG_DIR
    };
    let Ok(content) = gpg.dearmor(data) else {
        log.warning(SOURCE, &format!("Gpg error, failed to add key: {data}"));
        return "/dev/null".to_owned();
    };
    let path = format!("{key_dir}{output_file}.gpg");
    steps.push(Step::WriteKey {
        path: path.clone(),
        content,
    });
    path
}

/// `add_apt_key_raw`.
fn add_apt_key_raw(
    key: &str,
    file_name: &str,
    gpg: &mut dyn Gpg,
    hardened: bool,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> String {
    log.debug(SOURCE, &format!("Adding key:\n'{key}'"));
    apt_key_add(gpg, &path_stem(file_name), key, hardened, steps, log)
}

/// `add_apt_key`: resolve a `keyid` to a key, then place whatever key we have.
///
/// Mutates `ent` the way upstream does — the fetched key is written back under
/// `key`, so a later pass sees it.
///
/// # Errors
///
/// A `keyid` with no `filename` to write it to.
fn add_apt_key(
    ent: &mut Object,
    gpg: &mut dyn Gpg,
    hardened: bool,
    file_name: Option<&str>,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> Result<Option<String>, String> {
    if ent.contains_key("keyid") && !ent.contains_key("key") {
        let keyserver = ent
            .get("keyserver")
            .map_or_else(|| ci_gpg::DEFAULT_KEYSERVER.to_owned(), py_str);
        let keyid = ent.get("keyid").map_or_else(String::new, py_str);
        let fetched = gpg.getkeybyid(&keyid, &keyserver, log)?;
        // Upstream stores the result unconditionally, `None` included, and the
        // `"key" in ent` below then passes.
        ent.insert("key".to_owned(), fetched.map_or(Value::Null, Value::String));
    }
    let Some(key) = ent.get("key") else {
        return Ok(None);
    };
    let key = key.as_str().unwrap_or_default().to_owned();
    let name = match file_name {
        Some(name) => name.to_owned(),
        None => ent
            .get("filename")
            .map(py_str)
            .ok_or_else(|| "'filename'".to_owned())?,
    };
    Ok(Some(add_apt_key_raw(
        &key, &name, gpg, hardened, steps, log,
    )))
}

/// `add_mirror_keys`: the keys named inside `primary`/`security`.
///
/// The returned mapping feeds `primary_key`/`security_key` into the sources
/// template.
///
/// # Errors
///
/// Propagates a gpg failure that upstream lets escape.
pub fn add_mirror_keys(
    cfg: &mut Object,
    gpg: &mut dyn Gpg,
    steps: &mut Vec<Step>,
    log: &mut Logger,
) -> Result<Object, String> {
    let mut keys = Object::new();
    for key in ["primary", "security"] {
        let Some(mirrors) = cfg.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        for mirror in mirrors.iter_mut() {
            let Some(mirror) = mirror.as_object_mut() else {
                continue;
            };
            if let Some(resp) = add_apt_key(mirror, gpg, false, Some(key), steps, log)?
            {
                if !resp.is_empty() {
                    keys.insert(format!("{key}_key"), Value::String(resp));
                }
            }
        }
    }
    Ok(keys)
}

/// `_ensure_dependencies`: the packages the rest of this module will need.
///
/// Done up front because `install_packages` drags an `apt update` along with
/// it, and doing that twice doubles the cost of the whole module.
pub fn ensure_dependencies(
    cfg: &Object,
    aa_repo_match: &mut dyn FnMut(&str) -> bool,
    which: &mut dyn FnMut(&str) -> bool,
) -> Vec<String> {
    let mut required: Vec<&str> = Vec::new();
    let require = |cmd: &'static str, required: &mut Vec<&'static str>| {
        if !required.contains(&cmd) {
            required.push(cmd);
        }
    };
    let preserve = cfg
        .get("preserve_sources_list")
        .cloned()
        .unwrap_or(Value::Bool(false));
    if option::is_false(&preserve) {
        for mirror_key in ["primary", "security"] {
            let Some(items) = cfg
                .get(mirror_key)
                .filter(|value| option::py_truthy(value))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for item in items {
                if let Some(item) = item.as_object() {
                    if item.contains_key("key") || item.contains_key("keyid") {
                        require("gpg", &mut required);
                    }
                }
            }
        }
    }
    if let Some(sources) = cfg.get("sources").and_then(Value::as_object) {
        for ent in sources.values() {
            let Some(ent) = ent.as_object() else { continue };
            if ent.contains_key("key") || ent.contains_key("keyid") {
                require("gpg", &mut required);
            }
            let source = ent.get("source").map(py_str).unwrap_or_default();
            if aa_repo_match(&source) {
                require("add-apt-repository", &mut required);
            }
        }
    }
    let mut missing: Vec<String> = required
        .iter()
        .filter(|command| !which(command))
        .filter_map(|command| {
            PACKAGE_DEPENDENCY_BY_COMMAND
                .iter()
                .find(|(name, _)| name == command)
                .map(|(_, package)| (*package).to_owned())
        })
        .collect();
    missing.sort();
    missing
}

/// `add_apt_sources`: the `apt.sources` entries.
///
/// deb822 is deliberately unsupported here — upstream says so in its docstring.
///
/// # Errors
///
/// A non-mapping `sources`, an entry that is not a mapping, a gpg failure, or a
/// source line that will not render.
pub fn plan_apt_sources(
    srcdict: &Value,
    gpg: &mut dyn Gpg,
    template_params: &mut Object,
    aa_repo_match: &mut dyn FnMut(&str) -> bool,
    log: &mut Logger,
) -> Result<Vec<Step>, String> {
    let Some(srcdict) = srcdict.as_object() else {
        // `"unknown apt format: %s" % (srcdict)` — `%s`, so a string arrives
        // without quotes around it.
        return Err(format!("unknown apt format: {}", py_str(srcdict)));
    };
    let mut steps = Vec::new();
    for (filename, ent) in srcdict {
        log.debug(
            SOURCE,
            &format!("adding source/key '{}'", ci_config::repr(ent)),
        );
        let Some(ent) = ent.as_object() else {
            return Err(format!(
                "'{}' object does not support item assignment",
                ci_config::type_name(ent)
            ));
        };
        let mut ent = ent.clone();
        ent.entry("filename".to_owned())
            .or_insert_with(|| Value::String(filename.clone()));

        let source = ent.get("source").map(py_str);
        if source.as_deref().is_some_and(|s| s.contains("$KEY_FILE")) {
            let key_file = add_apt_key(&mut ent, gpg, true, None, &mut steps, log)?;
            template_params.insert(
                "KEY_FILE".to_owned(),
                Value::String(key_file.unwrap_or_default()),
            );
        } else {
            add_apt_key(&mut ent, gpg, false, None, &mut steps, log)?;
        }

        let Some(source) = source else { continue };
        let source = ci_template::render_string(
            &source,
            &Value::Object(template_params.clone()),
        )
        .map_err(|err| err.to_string())?;

        let mut path = ent.get("filename").map(py_str).unwrap_or_default();
        if !path.starts_with('/') {
            path = format!("/etc/apt/sources.list.d/{path}");
        }
        // A plain suffix test upstream, not an extension test: `.LIST` does
        // not count and gains a second suffix.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if !path.ends_with(".list") {
            path.push_str(".list");
        }

        if aa_repo_match(&source) {
            steps.push(Step::AddAptRepository { source });
            continue;
        }
        // `"append" in ent and not ent["append"]` — a missing key means append.
        let append = ent.get("append").is_none_or(option::py_truthy);
        steps.push(Step::WriteSource {
            path,
            content: format!("{source}\n"),
            append,
        });
    }
    steps.push(Step::UpdatePackageSources);
    Ok(steps)
}

/// `find_apt_mirror_info`.
///
/// # Errors
///
/// An arch with no default mirrors, or a `package_mirrors` template that will
/// not format.
#[allow(clippy::too_many_arguments)]
pub fn find_apt_mirror_info(
    cfg: &Object,
    arch: &str,
    datasource_mirrors: &mut dyn FnMut(&mut Logger) -> Result<Object, String>,
    search: &mut dyn FnMut(&[String], &mut Logger) -> Option<String>,
    search_dns: &mut dyn FnMut(&Value, &str, &mut Logger) -> Option<String>,
    log: &mut Logger,
) -> Result<Object, String> {
    let pmirror = get_mirror(cfg, "primary", arch, search, search_dns, log);
    log.debug(
        SOURCE,
        &format!("got primary mirror: {}", opt_repr(pmirror.as_deref())),
    );
    let smirror = get_mirror(cfg, "security", arch, search, search_dns, log);
    log.debug(
        SOURCE,
        &format!("got security mirror: {}", opt_repr(smirror.as_deref())),
    );
    // Asked for here rather than by the caller: it logs, and upstream's line
    // falls between the two above and the result below.
    let from_datasource = datasource_mirrors(log)?;
    let mut mirror_info = update_mirror_info(
        pmirror.as_deref(),
        smirror.as_deref(),
        arch,
        Some(&from_datasource),
    )?;
    // Less complex replacements use only MIRROR, derived from primary.
    let primary = mirror_info.get("PRIMARY").cloned().unwrap_or(Value::Null);
    mirror_info.insert("MIRROR".to_owned(), primary);
    Ok(mirror_info)
}

/// Python's `"%s" %` of an optional string.
fn opt_repr(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_owned(), ToOwned::to_owned)
}

/// `util.get_installed_packages`: the `hi`/`ii` rows of `dpkg-query --list`.
pub fn installed_packages(stdout: &str) -> Vec<String> {
    let mut packages: Vec<String> = Vec::new();
    for line in ci_core::pystr::split_lines(stdout) {
        let fields = ci_core::pystr::split_whitespace_n(line, 2);
        // Upstream's `(state, pkg, _) = line.split(None, 2)` needs all three.
        let (Some(state), Some(pkg)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if fields.len() < 3 {
            continue;
        }
        if !(state.starts_with("hi") || state.starts_with("ii")) {
            continue;
        }
        // `re.sub(":.*", "", pkg)` strips the multiarch qualifier.
        let name = pkg.split(':').next().unwrap_or_default().to_owned();
        if !packages.contains(&name) {
            packages.push(name);
        }
    }
    packages.sort();
    packages
}

/// What `apply_debconf_selections` decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebconfPlan {
    /// Fed to `debconf-set-selections` on stdin.
    pub selections: Vec<u8>,
    /// Packages to hand to `dpkg-reconfigure`, after their cleaner ran.
    pub reconfigure: Vec<String>,
    /// Packages that were preseeded but have no cleaner.
    pub unhandled: Vec<String>,
}

/// `apply_debconf_selections` + `dpkg_reconfigure`, deciding only.
///
/// # Errors
///
/// A `debconf_selections` that is not a mapping.
pub fn plan_debconf_selections(
    cfg: &Object,
    installed: &[String],
    log: &mut Logger,
) -> Result<Option<DebconfPlan>, String> {
    let Some(selsets) = cfg
        .get("debconf_selections")
        .filter(|value| option::py_truthy(value))
    else {
        log.debug(SOURCE, "debconf_selections was not set in config");
        return Ok(None);
    };
    let Some(selsets) = selsets.as_object() else {
        return Err(format!(
            "'{}' object has no attribute 'keys'",
            ci_config::type_name(selsets)
        ));
    };

    let mut keys: Vec<&String> = selsets.keys().collect();
    keys.sort();
    let joined = keys
        .iter()
        .filter_map(|key| selsets.get(*key))
        .map(py_str)
        .collect::<Vec<_>>()
        .join("\n");
    let mut selections = joined.into_bytes();
    if !selections.ends_with(b"\n") {
        selections.push(b'\n');
    }

    // Insertion order, not sorted: this set only feeds an intersection.
    let mut configured: Vec<String> = Vec::new();
    for content in selsets.values() {
        for line in ci_core::pystr::split_lines(&py_str(content)) {
            if line.starts_with('#') {
                continue;
            }
            // `re.sub(r"[:\s].*", "", line)` keeps the head of the line.
            let name = line
                .split(|c: char| c == ':' || c.is_whitespace())
                .next()
                .unwrap_or_default()
                .to_owned();
            if !configured.contains(&name) {
                configured.push(name);
            }
        }
    }
    log.debug(SOURCE, &format!("pkgs_cfgd: {}", repr_set(&configured)));

    let mut need_reconfig: Vec<String> = configured
        .iter()
        .filter(|name| installed.contains(name))
        .cloned()
        .collect();
    need_reconfig.sort();
    if need_reconfig.is_empty() {
        log.debug(SOURCE, "no need for reconfig");
        return Ok(Some(DebconfPlan {
            selections,
            reconfigure: Vec::new(),
            unhandled: Vec::new(),
        }));
    }

    // `CONFIG_CLEANERS` has exactly one entry.
    let (reconfigure, unhandled): (Vec<String>, Vec<String>) = need_reconfig
        .into_iter()
        .partition(|name| name == "cloud-init");
    for name in &reconfigure {
        log.debug(SOURCE, &format!("unconfiguring {name}"));
    }
    if !unhandled.is_empty() {
        log.warning(
            SOURCE,
            &format!(
                "The following packages were installed and preseeded, \
                 but cannot be unconfigured: {}",
                repr_list(&unhandled)
            ),
        );
    }
    Ok(Some(DebconfPlan {
        selections,
        reconfigure,
        unhandled,
    }))
}

/// Python's `"%s" %` of a list of strings.
fn repr_list(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|item| ci_config::repr_str(item))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

/// Python's `"%s" %` of a set of strings.
fn repr_set(items: &[String]) -> String {
    if items.is_empty() {
        return "set()".to_owned();
    }
    let inner = items
        .iter()
        .map(|item| ci_config::repr_str(item))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{inner}}}")
}

/// One `rename_apt_lists` pair: every `<from>_*` becomes `<to>_*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRename {
    pub from_prefix: String,
    pub to_prefix: String,
}

/// `rename_apt_lists`: keep apt's cache usable across a mirror change.
///
/// # Errors
///
/// An arch with no default mirrors.
pub fn plan_apt_list_renames(
    new_mirrors: &Object,
    arch: &str,
    lists_dir: &str,
) -> Result<Vec<ListRename>, String> {
    let mut renames = Vec::new();
    for (name, omirror) in get_default_mirrors(arch)? {
        let Some(nmirror) = new_mirrors
            .get(&name)
            .filter(|value| option::py_truthy(value))
            .map(py_str)
        else {
            continue;
        };
        let from_prefix =
            format!("{lists_dir}/{}", mirrorurl_to_apt_fileprefix(&omirror));
        let to_prefix =
            format!("{lists_dir}/{}", mirrorurl_to_apt_fileprefix(&nmirror));
        if from_prefix == to_prefix {
            continue;
        }
        renames.push(ListRename {
            from_prefix,
            to_prefix,
        });
    }
    Ok(renames)
}

/// `mirrorurl_to_apt_fileprefix`: the on-disk spelling apt caches a mirror as.
pub fn mirrorurl_to_apt_fileprefix(mirror: &str) -> String {
    let mut string = mirror;
    if let Some(stripped) = string.strip_suffix('/') {
        string = stripped;
    }
    if let Some(pos) = string.find("://") {
        string = string.get(pos + 3..).unwrap_or("");
    }
    string.replace('/', "_")
}

// ---------------------------------------------------------------------------
// the module body
// ---------------------------------------------------------------------------

/// `type(x)` as Python prints it, for the one message that embeds it.
fn py_type(value: &Value) -> String {
    format!("<class '{}'>", ci_config::type_name(value))
}

/// `util.rand_str(strlen=8)` over `string.ascii_letters + string.digits`.
///
/// Upstream draws each character separately from `random.SystemRandom`; this
/// takes ten out of each 64-bit draw, which is very slightly biased towards the
/// front of the alphabet. The result only has to be a mapping key nobody else
/// picked, so the bias costs nothing.
fn rand_str(strlen: usize) -> String {
    const ALPHABET: &[u8; 62] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = String::with_capacity(strlen);
    while out.len() < strlen {
        let mut draw = ci_sys::rand::u64();
        for _ in 0..10 {
            if out.len() == strlen {
                break;
            }
            let index = usize::try_from(draw % 62).unwrap_or(0);
            if let Some(&byte) = ALPHABET.get(index) {
                out.push(char::from(byte));
            }
            draw /= 62;
        }
    }
    out
}

/// `util.rand_dict_key`.
fn rand_dict_key(dictionary: &Object, postfix: &str) -> String {
    loop {
        let candidate = format!("{}_{postfix}", rand_str(8));
        if !dictionary.contains_key(&candidate) {
            return candidate;
        }
    }
}

/// `glob.glob`, for the two patterns this module uses.
fn glob(pattern: &str) -> Vec<std::path::PathBuf> {
    let path = std::path::Path::new(pattern);
    let (Some(parent), Some(name)) =
        (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Vec::new();
    };
    if !name.contains('*') {
        return if path.exists() {
            vec![path.to_owned()]
        } else {
            Vec::new()
        };
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|found| !found.starts_with('.') && star_match(name, found))
        })
        .map(|entry| entry.path())
        .collect();
    // `glob.glob` returns directory order; sorting only makes runs repeatable.
    out.sort();
    out
}

/// `fnmatch` for a pattern whose only metacharacter is `*`.
fn star_match(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = rest.get(at + part.len()..).unwrap_or("");
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// The `netloc` of a URL, minus any userinfo, port and IPv6 brackets.
fn url_hostname(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.split_once(']').map(|(addr, _)| addr)?,
        None => host.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// The addresses a wildcard resolver hands back for names that cannot exist.
///
/// `util.is_resolvable` caches this in a module global and so computes it once
/// per process; a `OnceLock` is the same thing said differently.
fn dns_redirect_ips(log: &mut Logger) -> &'static Vec<std::net::IpAddr> {
    const BAD_NAMES: [&str; 3] = [
        "does-not-exist.example.com.",
        "example.invalid.",
        "__cloud_init_expected_not_found__",
    ];
    static CACHE: std::sync::OnceLock<Vec<std::net::IpAddr>> =
        std::sync::OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return cached;
    }
    let mut bad: Vec<std::net::IpAddr> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for name in BAD_NAMES {
        for addr in resolve(name) {
            results.push(format!("{name}: {addr}"));
            if !bad.contains(&addr) {
                bad.push(addr);
            }
        }
    }
    if !results.is_empty() {
        // Upstream also prints each answer's canonical name, which it asks for
        // with `AI_CANONNAME`; `to_socket_addrs` does not expose one.
        log.debug(
            "util.py",
            &format!("detected dns redirection: {}", repr_list(&results)),
        );
    }
    CACHE.get_or_init(|| bad)
}

/// `socket.getaddrinfo(name, None)`, reduced to the addresses.
fn resolve(name: &str) -> Vec<std::net::IpAddr> {
    use std::net::ToSocketAddrs as _;

    (name, 0u16)
        .to_socket_addrs()
        .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        .unwrap_or_default()
}

/// `util.is_resolvable`: does this URL's host resolve to something real?
fn is_resolvable(url: &str, log: &mut Logger) -> bool {
    let Some(name) = url_hostname(url) else {
        return false;
    };
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let bad = dns_redirect_ips(log);
    // Upstream checks only the *first* answer against the redirect set.
    resolve(&name)
        .first()
        .is_some_and(|addr| !bad.contains(addr))
}

/// `util.search_for_mirror`.
fn search_for_mirror(candidates: &[String], log: &mut Logger) -> Option<String> {
    log.debug(
        "util.py",
        &format!(
            "search for mirror in candidates: '{}'",
            repr_list(candidates)
        ),
    );
    for candidate in candidates {
        if is_resolvable(candidate, log) {
            log.debug("util.py", &format!("found working mirror: '{candidate}'"));
            return Some(candidate.clone());
        }
    }
    None
}

/// `_should_configure_on_empty_apt`, as the refusal or nothing.
fn refuse_on_empty_apt(root: &std::path::Path) -> Option<&'static str> {
    if ci_core::sysinfo::system_is_snappy(root) {
        return Some("system is snappy.");
    }
    if ci_sys::subp::which("apt-get").is_none() && ci_sys::subp::which("apt").is_none()
    {
        return Some("no apt commands.");
    }
    None
}

/// A logical path resolved against the tree the module is acting on.
fn at(root: &std::path::Path, logical: &str) -> std::path::PathBuf {
    root.join(logical.trim_start_matches('/'))
}

/// `subp.subp`, refused outside a live root the way the other modules refuse it.
fn subp(argv: &[String], stdin: Option<&[u8]>, live: bool) -> Result<(), String> {
    if !live {
        return Err(format!(
            "refusing to run {:?} against a rooted tree",
            argv.first().map_or("", String::as_str)
        ));
    }
    let mut command = ci_sys::subp::Subp::new(argv).inherit_env();
    if let Some(data) = stdin {
        command = command.stdin(data.to_vec());
    }
    command.check().map(|_| ()).map_err(|err| err.to_string())
}

/// `util.write_file` / `util.del_file` for one planned change.
fn run_file_action(
    action: &FileAction,
    root: &std::path::Path,
    log: &mut Logger,
) -> std::io::Result<()> {
    match action {
        FileAction::Write {
            path,
            content,
            mode,
        } => {
            let target = at(root, path);
            if let Some(parent) = target.parent() {
                ci_sys::path::ensure_dir(parent, 0o755)?;
            }
            log.debug(
                "util.py",
                &format!(
                    "Writing to {} - wb: [{}] {} bytes",
                    target.display(),
                    mode.unwrap_or(0o644),
                    content.len()
                ),
            );
            ci_sys::atomic::write_file(
                &target,
                content,
                ci_sys::atomic::WriteOptions::mode(mode.unwrap_or(0o644)),
            )
        }
        FileAction::Remove(path) => match std::fs::remove_file(at(root, path)) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    }
}

/// `util.get_installed_packages`.
fn query_installed_packages(live: bool) -> Result<Vec<String>, String> {
    if !live {
        return Ok(Vec::new());
    }
    let argv = ["dpkg-query", "--list"].map(ToOwned::to_owned);
    let output = ci_sys::subp::Subp::new(argv)
        .inherit_env()
        .check()
        .map_err(|err| err.to_string())?;
    Ok(installed_packages(&String::from_utf8_lossy(&output.stdout)))
}

/// `get_apt_cfg`'s `apt-config dump` fallback. The `apt_pkg` branch above it
/// needs a Python interpreter, so this is the only one available here.
fn apt_config_dump() -> Option<String> {
    let argv = ["apt-config", "dump"].map(ToOwned::to_owned);
    let output = ci_sys::subp::Subp::new(argv).inherit_env().check().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `clean_cloud_init`.
fn clean_cloud_init(root: &std::path::Path, log: &mut Logger) {
    let pattern = at(root, "/etc/cloud/cloud.cfg.d/*dpkg*");
    let flist = glob(&pattern.to_string_lossy());
    let shown: Vec<String> = flist
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    log.debug(
        SOURCE,
        &format!("cleaning cloud-init config from: {}", repr_list(&shown)),
    );
    for path in flist {
        let _ = std::fs::remove_file(path);
    }
}

/// `apply_debconf_selections` and the `dpkg_reconfigure` it ends in.
fn apply_debconf_selections(
    cfg: &Object,
    args: &mut Args<'_>,
    live: bool,
) -> Result<(), String> {
    // Checked here as well as in the planner so that a config without the key
    // does not pay for a `dpkg-query` it will not look at.
    if !cfg.get("debconf_selections").is_some_and(option::py_truthy) {
        args.logger
            .debug(SOURCE, "debconf_selections was not set in config");
        return Ok(());
    }
    // Upstream pushes the selections *before* it reads the installed set. The
    // order is reversed here because the planner needs both at once, and
    // `debconf-set-selections` cannot install or remove a package, so the
    // answer is the same either way.
    let installed = query_installed_packages(live)?;
    let Some(plan) = plan_debconf_selections(cfg, &installed, args.logger)? else {
        return Ok(());
    };
    subp(
        &["debconf-set-selections".to_owned()],
        Some(&plan.selections),
        live,
    )?;
    if plan.reconfigure.is_empty() {
        return Ok(());
    }
    for name in &plan.reconfigure {
        // `CONFIG_CLEANERS` has exactly one entry, and the planner only ever
        // puts that one in `reconfigure`.
        if name == "cloud-init" {
            clean_cloud_init(args.root, args.logger);
        }
    }
    let mut command_line = vec![
        "dpkg-reconfigure".to_owned(),
        "--frontend=noninteractive".to_owned(),
    ];
    command_line.extend(plan.reconfigure.iter().cloned());
    subp(&command_line, None, live)
}

/// `cloud.distro.install_packages`.
fn install_packages(
    names: &[String],
    args: &mut Args<'_>,
    live: bool,
) -> Result<(), String> {
    let managers = args.distro.package_managers;
    let state =
        super::package_update_upgrade_install::State::probe(args.root, managers);
    let config =
        ci_distro::packages::AptConfig::from_config(args.system_info, &mut |name| {
            ci_sys::subp::which(name).is_some()
        })?;
    let pkglist: Vec<Value> = names.iter().cloned().map(Value::String).collect();
    let plan = ci_distro::packages::plan_install(
        managers,
        &config,
        &state.packages,
        &pkglist,
        args.logger,
    )?;
    run_package_steps(&plan.steps, live)?;
    plan.failed.clone().map_or(Ok(()), Err)
}

fn run_package_steps(
    steps: &[ci_distro::packages::Step],
    live: bool,
) -> Result<(), String> {
    ci_distro::packages::run(steps, &mut |step| {
        if !live {
            return Err(format!(
                "refusing to run {:?} against a rooted tree",
                step.argv.first().map_or("", String::as_str)
            ));
        }
        let mut command = ci_sys::subp::Subp::new(step.argv.clone()).inherit_env();
        for (key, value) in &step.env {
            command = command.env(key, value);
        }
        command.check().map(|_| ()).map_err(|err| err.to_string())
    })
}

/// Carry out the [`Step`]s `add_apt_sources` planned.
fn run_steps(steps: &[Step], args: &mut Args<'_>, live: bool) -> Result<(), String> {
    for step in steps {
        match step {
            Step::WriteKey { path, content } => {
                let target = at(args.root, path);
                if let Some(parent) = target.parent() {
                    ci_sys::path::ensure_dir(parent, 0o755)
                        .map_err(|err| err.to_string())?;
                }
                ci_sys::atomic::write_file(
                    &target,
                    content,
                    ci_sys::atomic::WriteOptions::mode(0o644),
                )
                .map_err(|err| err.to_string())?;
            }
            Step::WriteSource {
                path,
                content,
                append,
            } => {
                let target = at(args.root, path);
                if let Some(parent) = target.parent() {
                    ci_sys::path::ensure_dir(parent, 0o755)
                        .map_err(|err| err.to_string())?;
                }
                let result = if *append {
                    ci_sys::atomic::append_file(&target, content, 0o644)
                } else {
                    ci_sys::atomic::write_file(
                        &target,
                        content,
                        ci_sys::atomic::WriteOptions::mode(0o644),
                    )
                };
                result.map_err(|err| {
                    args.logger.warning(
                        SOURCE,
                        &format!("failed write to file {}: {err}", target.display()),
                    );
                    err.to_string()
                })?;
            }
            Step::AddAptRepository { source } => {
                let command_line = [
                    "add-apt-repository".to_owned(),
                    "--no-update".to_owned(),
                    source.clone(),
                ];
                subp(&command_line, None, live).inspect_err(|_| {
                    args.logger.warning(SOURCE, "add-apt-repository failed.");
                })?;
            }
            Step::UpdatePackageSources => {
                let managers = args.distro.package_managers;
                let state = super::package_update_upgrade_install::State::probe(
                    args.root, managers,
                );
                let config = ci_distro::packages::AptConfig::from_config(
                    args.system_info,
                    &mut |name| ci_sys::subp::which(name).is_some(),
                )?;
                let plan = ci_distro::packages::plan_update_sources(
                    managers,
                    &config,
                    &state.packages,
                    true,
                    args.logger,
                );
                run_package_steps(&plan, live)?;
            }
        }
    }
    Ok(())
}

/// `find_apt_mirror_info` with the live probes wired to it.
fn resolve_mirrors(
    cfg: &Object,
    arch: &str,
    distro: &str,
    ds: &DatasourcePlacement<'_>,
    log: &mut Logger,
) -> Result<Object, String> {
    let mut search = search_for_mirror;
    let mut search_dns = |configured: &Value, mirrortype: &str, log: &mut Logger| {
        if !option::py_truthy(configured) {
            return None;
        }
        // The `ValueError` for an unknown mirror type is unreachable:
        // `find_apt_mirror_info` only ever asks for these two.
        let candidates = mirror_dns_candidates(mirrortype, ds.fqdn, distro).ok()?;
        search_for_mirror(&candidates, log)
    };
    // `cloud.datasource.get_package_mirror_info()`, which is what makes an
    // image's own `system_info.package_mirrors` reach apt.
    let mut from_datasource = |log: &mut Logger| {
        let arch_info = ds.package_mirrors.and_then(|items| {
            ci_distro::mirrors::arch_package_mirror_info(items, arch)
        });
        ci_distro::mirrors::package_mirror_info(
            arch_info,
            ds.availability_zone,
            ds.region,
            ds.platform_type,
            &mut search_for_mirror,
            log,
        )
    };
    find_apt_mirror_info(
        cfg,
        arch,
        &mut from_datasource,
        &mut search,
        &mut search_dns,
        log,
    )
}

/// `DataSource.availability_zone`: the top-level key under either spelling,
/// else the one nested under `placement`.
fn availability_zone(metadata: &Object) -> Option<&str> {
    let top = metadata
        .get("availability-zone")
        .or_else(|| metadata.get("availability_zone"))
        .and_then(Value::as_str);
    if let Some(zone) = top.filter(|zone| !zone.is_empty()) {
        return Some(zone);
    }
    metadata
        .get("placement")
        .and_then(Value::as_object)?
        .get("availability-zone")
        .and_then(Value::as_str)
}

/// The facts about where this instance is running that the mirror search
/// substitutes into `system_info.package_mirrors`.
struct DatasourcePlacement<'a> {
    fqdn: &'a str,
    availability_zone: Option<&'a str>,
    region: Option<&'a str>,
    platform_type: &'a str,
    package_mirrors: Option<&'a Vec<Value>>,
}

/// The `preserve_sources_list` branch: mirror keys, the rendered template and
/// the cached-list renames that follow the mirrors moving.
fn regenerate_sources_list(
    cfg: &mut Object,
    gpg: &mut dyn Gpg,
    mirrors: &Object,
    release: &str,
    arch: &str,
    args: &mut Args<'_>,
    live: bool,
) -> Result<(), String> {
    let mut steps = Vec::new();
    let keys = add_mirror_keys(cfg, gpg, &mut steps, args.logger)?;
    run_steps(&steps, args, live)?;

    let apt_paths = get_apt_cfg(apt_config_dump().as_deref());
    let templates_dir = args.paths.templates_dir.clone();
    let root = args.root.to_path_buf();
    let mut load_template = |name: &str, log: &mut Logger| {
        let path = templates_dir.join(format!("{name}.tmpl"));
        if !path.is_file() {
            log.warning(
                "cloud.py",
                &format!(
                    "No template found in {} for template named {name}",
                    path.parent().unwrap_or(&templates_dir).display()
                ),
            );
            return None;
        }
        std::fs::read_to_string(path).ok()
    };
    let mut read_file = |path: &str| std::fs::read_to_string(at(&root, path)).ok();
    let actions = plan_sources_list(
        cfg,
        release,
        mirrors,
        args.distro.name,
        &keys,
        ci_core::features::APT_DEB822_SOURCE_LIST_FILE,
        &apt_paths,
        &mut load_template,
        &mut read_file,
        args.logger,
    )?;
    for action in &actions {
        run_file_action(action, args.root, args.logger)
            .map_err(|err| err.to_string())?;
    }
    rename_apt_lists(mirrors, arch, args)
}

/// `apply_apt`.
fn apply_apt(
    cfg: &mut Object,
    gpg: &mut dyn Gpg,
    args: &mut Args<'_>,
    live: bool,
) -> Result<(), String> {
    if cfg.is_empty() {
        if let Some(reason) = refuse_on_empty_apt(args.root) {
            args.logger.debug(
                SOURCE,
                &format!("Nothing to do: No apt config and {reason}"),
            );
            return Ok(());
        }
    }
    let shown = ci_config::repr(&Value::Object(cfg.clone()));
    args.logger
        .debug(SOURCE, &format!("handling apt config: {shown}"));

    let release = ci_distro::probe::lsb_release(args.logger)
        .get("codename")
        .cloned()
        // `lsb_release()["codename"]` on a mapping that has no such key.
        .ok_or_else(|| "'codename'".to_owned())?;
    // `Distro.get_primary_arch`, which the Debian family overrides to the
    // dpkg architecture — the same string `apply_apt` already asked for.
    let arch = ci_distro::probe::get_dpkg_architecture()?;
    let fqdn = ci_core::hostname::get_hostname_fqdn(
        args.cfg,
        args.datasource.map(|ds| ds.metadata),
        args.root,
    )
    .fqdn;
    let metadata = args.datasource.map(|ds| ds.metadata);
    let placement = DatasourcePlacement {
        fqdn: &fqdn,
        availability_zone: metadata.and_then(availability_zone),
        region: metadata
            .and_then(|metadata| metadata.get("region").and_then(Value::as_str)),
        // `DataSource.platform_type` defaults to the lowercased dsname.
        platform_type: args.datasource.map_or("", |ds| ds.dsname),
        package_mirrors: args
            .system_info
            .get("package_mirrors")
            .and_then(Value::as_array),
    };

    let mirrors =
        resolve_mirrors(cfg, &arch, args.distro.name, &placement, args.logger)?;
    let shown = ci_config::repr(&Value::Object(mirrors.clone()));
    args.logger
        .debug(SOURCE, &format!("Apt Mirror info: {shown}"));

    let matchcfg = cfg
        .get("add_apt_repo_match")
        .cloned()
        .unwrap_or_else(|| Value::String(ADD_APT_REPO_MATCH.to_owned()));
    let matcher = if option::py_truthy(&matchcfg) {
        Some(regex::Regex::new(&py_str(&matchcfg)).map_err(|err| err.to_string())?)
    } else {
        None
    };
    let has_sources = cfg
        .get("sources")
        .and_then(Value::as_object)
        .is_some_and(|sources| !sources.is_empty());
    if matcher.is_none() && has_sources {
        // Upstream passes `matcher = None` straight into `_ensure_dependencies`,
        // which calls it. Falsy `add_apt_repo_match` plus any `sources` entry is
        // therefore a `TypeError`, not a way to switch the matching off.
        return Err("'NoneType' object is not callable".to_owned());
    }
    let mut aa_repo_match =
        |source: &str| matcher.as_ref().is_some_and(|re| re.is_match(source));

    let missing = ensure_dependencies(cfg, &mut aa_repo_match, &mut |name| {
        ci_sys::subp::which(name).is_some()
    });
    if !missing.is_empty() {
        install_packages(&missing, args, live)?;
    }

    let preserve = cfg
        .get("preserve_sources_list")
        .cloned()
        .unwrap_or(Value::Bool(false));
    if option::is_false(&preserve) {
        regenerate_sources_list(cfg, gpg, &mirrors, &release, &arch, args, live)?;
    }

    let mut exists = |path: &str| at(args.root, path).is_file();
    let actions =
        plan_apt_config(cfg, APT_PROXY_FN, APT_CONFIG_FN, &mut exists, args.logger);
    for action in &actions {
        if let Err(err) = run_file_action(action, args.root, args.logger) {
            // `except (IOError, OSError)` — a failure here is logged, not fatal.
            args.logger.warning(
                SOURCE,
                &format!("Failed to apply proxy or apt config info: {err}"),
            );
            break;
        }
    }

    if let Some(sources) = cfg.get("sources").cloned() {
        let mut params = mirrors;
        params.insert("RELEASE".to_owned(), Value::String(release));
        // `params["MIRROR"] = mirrors["MIRROR"]`, which `find_apt_mirror_info`
        // already put there.
        let steps = plan_apt_sources(
            &sources,
            gpg,
            &mut params,
            &mut aa_repo_match,
            args.logger,
        )?;
        run_steps(&steps, args, live)?;
    }
    Ok(())
}

/// `rename_apt_lists`.
fn rename_apt_lists(
    mirrors: &Object,
    arch: &str,
    args: &mut Args<'_>,
) -> Result<(), String> {
    let lists = at(args.root, APT_LISTS);
    for rename in plan_apt_list_renames(mirrors, arch, &lists.to_string_lossy())? {
        for path in glob(&format!("{}_*", rename.from_prefix)) {
            let name = path.to_string_lossy();
            let Some(tail) = name.get(rename.from_prefix.len()..) else {
                continue;
            };
            let newname = format!("{}{tail}", rename.to_prefix);
            args.logger
                .debug(SOURCE, &format!("Renaming apt list {name} to {newname}"));
            if let Err(err) = std::fs::rename(&path, &newname) {
                // Best effort: upstream warns and carries on.
                args.logger
                    .warning(SOURCE, &format!("Failed to rename apt list: {err}"));
            }
        }
    }
    Ok(())
}

/// `handle`.
///
/// # Errors
///
/// A non-mapping `apt` block, a template that will not render, a gpg failure,
/// or a package install that did not take.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let mut cfg = args.cfg.clone();
    convert_to_v3_apt_format(&mut cfg, args.logger, rand_dict_key)?;
    let mut apt_cfg = match cfg.get("apt") {
        // `cfg.get("apt", {})` — an absent key, not an explicit null.
        None => Object::new(),
        Some(Value::Object(mapping)) => mapping.clone(),
        Some(other) => {
            return Err(format!(
                "Expected dictionary for 'apt' config, found {}",
                py_type(other)
            ))
        }
    };
    let live = args.root == std::path::Path::new("/");
    apply_debconf_selections(&apt_cfg, args, live)?;
    // `with GPG()`: the temporary keyring and the agent it starts are both
    // cleaned up when this drops, however the block below ends.
    let mut gpg = ci_gpg::RealGpg::new()?;
    apply_apt(&mut apt_cfg, &mut gpg, args, live)
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

    fn obj(text: &str) -> Object {
        ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default())
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    fn counter() -> impl FnMut(&Object, &str) -> String {
        let mut n = 0;
        move |_dict: &Object, postfix: &str| {
            n += 1;
            format!("key{n}_{postfix}")
        }
    }

    #[test]
    fn a_ports_arch_gets_the_ports_mirrors() {
        let mirrors = get_default_mirrors("arm64").unwrap();
        assert_eq!(mirrors[0].1, "http://ports.ubuntu.com/ubuntu-ports");
        let mirrors = get_default_mirrors("amd64").unwrap();
        assert_eq!(mirrors[0].1, "http://archive.ubuntu.com/ubuntu/");
        assert!(get_default_mirrors("m68k").is_err());
    }

    #[test]
    fn a_v1_source_list_becomes_a_dict_keyed_by_filename() {
        let mut log = Logger::silent();
        let srclist = Value::Array(vec![
            Value::Object(obj("filename: a.list\nsource: deb http://x/ y main")),
            Value::Object(obj("source: deb http://z/ y main")),
        ]);
        let out = convert_v1_to_v2_apt_format(&srclist, &mut log, counter()).unwrap();
        assert!(out.contains_key("a.list"));
        assert!(out.contains_key("key1_cloud_config_sources.list"));
        // The unnamed entry still gets the colliding filename written into it.
        assert_eq!(
            out["key1_cloud_config_sources.list"]["filename"],
            Value::String("cloud_config_sources.list".to_owned())
        );
    }

    #[test]
    fn the_flat_v2_keys_move_under_apt() {
        let mut log = Logger::silent();
        let mut cfg = obj("apt_proxy: http://proxy:3128/\napt_mirror: http://m/\n");
        convert_v2_to_v3_apt_format(&mut cfg, &mut log).unwrap();
        let apt = cfg["apt"].as_object().unwrap();
        assert_eq!(apt["proxy"], Value::String("http://proxy:3128/".to_owned()));
        assert_eq!(
            apt["primary"][0]["uri"],
            Value::String("http://m/".to_owned())
        );
        assert_eq!(
            apt["primary"][0]["arches"][0],
            Value::String("default".to_owned())
        );
        assert!(!cfg.contains_key("apt_proxy"));
    }

    /// Upstream transposes the ftp and https proxy names. See B76.
    #[test]
    fn the_ftp_and_https_proxy_keys_are_swapped_on_the_way_to_v3() {
        let mut log = Logger::silent();
        let mut cfg = obj("apt_ftp_proxy: ftp://f/\napt_https_proxy: https://h/\n");
        convert_v2_to_v3_apt_format(&mut cfg, &mut log).unwrap();
        let apt = cfg["apt"].as_object().unwrap();
        assert_eq!(apt["https_proxy"], Value::String("ftp://f/".to_owned()));
        assert_eq!(apt["ftp_proxy"], Value::String("https://h/".to_owned()));
    }

    #[test]
    fn an_empty_or_null_old_key_is_dropped_rather_than_converted() {
        let mut log = Logger::silent();
        let mut cfg = obj("apt_proxy: ''\napt_mirror: null\n");
        convert_v2_to_v3_apt_format(&mut cfg, &mut log).unwrap();
        assert!(!cfg.contains_key("apt_proxy"));
        assert!(!cfg.contains_key("apt_mirror"));
        // Nothing needed converting, so no `apt` key was invented.
        assert!(!cfg.contains_key("apt"));
    }

    #[test]
    fn old_and_new_keys_that_disagree_are_an_error() {
        let mut log = Logger::silent();
        let mut cfg = obj("apt_proxy: http://a/\napt:\n  proxy: http://b/\n");
        let err = convert_v2_to_v3_apt_format(&mut cfg, &mut log).unwrap_err();
        assert!(err.contains("unequal values"), "{err}");
    }

    #[test]
    fn old_and_new_keys_that_agree_leave_the_new_one_standing() {
        let mut log = Logger::silent();
        let mut cfg = obj("apt_proxy: http://a/\napt:\n  proxy: http://a/\n");
        convert_v2_to_v3_apt_format(&mut cfg, &mut log).unwrap();
        assert_eq!(cfg["apt"]["proxy"], Value::String("http://a/".to_owned()));
        assert!(!cfg.contains_key("apt_proxy"));
    }

    #[test]
    fn an_exact_arch_match_beats_the_default_entry() {
        let cfg = obj(
            "primary:\n  - arches: [default]\n    uri: http://default/\n\
             \n  - arches: [arm64]\n    uri: http://arm/\n",
        );
        let chosen = get_arch_mirrorconfig(&cfg, "primary", "arm64").unwrap();
        assert_eq!(chosen["uri"], Value::String("http://arm/".to_owned()));
        let chosen = get_arch_mirrorconfig(&cfg, "primary", "s390x").unwrap();
        assert_eq!(chosen["uri"], Value::String("http://default/".to_owned()));
        assert!(get_arch_mirrorconfig(&cfg, "security", "arm64").is_none());
    }

    #[test]
    fn a_uri_wins_over_search_which_wins_over_search_dns() {
        let mut log = Logger::silent();
        let cfg = obj(
            "primary:\n  - arches: [default]\n    uri: http://direct/\n    \
             search: [http://searched/]\n",
        );
        let found = get_mirror(
            &cfg,
            "primary",
            "arm64",
            &mut |_, _| Some("http://searched/".to_owned()),
            &mut |_, _, _| Some("http://dns/".to_owned()),
            &mut log,
        );
        assert_eq!(found.as_deref(), Some("http://direct/"));

        let cfg =
            obj("primary:\n  - arches: [default]\n    search: [http://searched/]\n");
        let found = get_mirror(
            &cfg,
            "primary",
            "arm64",
            &mut |candidates, _| candidates.first().cloned(),
            &mut |_, _, _| Some("http://dns/".to_owned()),
            &mut log,
        );
        assert_eq!(found.as_deref(), Some("http://searched/"));

        let cfg = obj("primary:\n  - arches: [default]\n    search_dns: true\n");
        let found = get_mirror(
            &cfg,
            "primary",
            "arm64",
            &mut |_, _| None,
            &mut |_, _, _| Some("http://dns/".to_owned()),
            &mut log,
        );
        assert_eq!(found.as_deref(), Some("http://dns/"));
    }

    #[test]
    fn security_defaults_to_primary_and_then_to_the_arch_default() {
        let info = update_mirror_info(Some("http://p/"), None, "arm64", None).unwrap();
        assert_eq!(info["SECURITY"], Value::String("http://p/".to_owned()));

        let info = update_mirror_info(None, None, "arm64", None).unwrap();
        assert_eq!(
            info["PRIMARY"],
            Value::String("http://ports.ubuntu.com/ubuntu-ports".to_owned())
        );

        let ds = obj("primary: http://ds-p/\nsecurity: http://ds-s/\n");
        let info = update_mirror_info(None, None, "arm64", Some(&ds)).unwrap();
        assert_eq!(info["PRIMARY"], Value::String("http://ds-p/".to_owned()));
        assert_eq!(info["SECURITY"], Value::String("http://ds-s/".to_owned()));
        // The lowercase keys survive alongside the uppercase ones.
        assert_eq!(info["primary"], Value::String("http://ds-p/".to_owned()));
    }

    #[test]
    fn the_dns_candidates_walk_domain_then_localdomain_then_bare() {
        let candidates =
            mirror_dns_candidates("primary", "host.example.com", "ubuntu").unwrap();
        assert_eq!(
            candidates,
            vec![
                "http://ubuntu-mirror.example.com/ubuntu",
                "http://ubuntu-mirror.localdomain/ubuntu",
                "http://ubuntu-mirror/ubuntu",
            ]
        );
        let candidates = mirror_dns_candidates("security", "host", "ubuntu").unwrap();
        assert_eq!(
            candidates,
            vec![
                "http://ubuntu-security-mirror.localdomain/ubuntu",
                "http://ubuntu-security-mirror/ubuntu",
            ]
        );
        assert!(mirror_dns_candidates("other", "host", "ubuntu").is_err());
    }

    #[test]
    fn the_suite_shorthands_expand_and_anything_else_passes_through() {
        assert_eq!(map_known_suites("updates"), "$RELEASE-updates");
        assert_eq!(map_known_suites("release"), "$RELEASE");
        assert_eq!(map_known_suites("noble-odd"), "noble-odd");
    }

    #[test]
    fn deb822_is_detected_by_its_keys_and_one_line_by_its_deb_prefix() {
        let mut log = Logger::silent();
        assert!(!is_deb822_sources_format(
            "deb http://x/ noble main",
            &mut log
        ));
        assert!(is_deb822_sources_format(
            "Types: deb\nURIs: http://x/\n",
            &mut log
        ));
        // Neither shape: warns and assumes the one-line format.
        assert!(!is_deb822_sources_format("# nothing here\n", &mut log));
    }

    #[test]
    fn a_mirror_url_becomes_its_apt_cache_prefix() {
        assert_eq!(
            mirrorurl_to_apt_fileprefix("http://archive.ubuntu.com/ubuntu/"),
            "archive.ubuntu.com_ubuntu"
        );
        assert_eq!(
            mirrorurl_to_apt_fileprefix("ports.ubuntu.com/ubuntu-ports"),
            "ports.ubuntu.com_ubuntu-ports"
        );
    }

    const DEB822_SRC: &str = "Types: deb\nURIs: http://x/\n\
         Suites: noble noble-updates noble-backports\nComponents: main\n\n\
         Types: deb\nURIs: http://sec/\nSuites: noble-security\nComponents: main\n";

    fn disabled(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Expected output taken from the installed cloud-init, not from reading.
    #[test]
    fn a_one_line_sources_file_gets_its_disabled_suites_commented_out() {
        let mut log = Logger::silent();
        let src = "deb http://x/ noble main\ndeb http://x/ noble-updates main\n\
                   # c\ndeb-src http://x/ noble-security main\n";
        let out =
            disable_suites(&disabled(&["updates", "security"]), src, "noble", &mut log)
                .unwrap();
        assert_eq!(
            out,
            "deb http://x/ noble main\n\
             # suite disabled by cloud-init: deb http://x/ noble-updates main\n\
             # c\n\
             # suite disabled by cloud-init: deb-src http://x/ noble-security main\n"
        );
    }

    #[test]
    fn a_deb822_suite_is_redacted_and_the_original_kept_as_a_comment() {
        let mut log = Logger::silent();
        let out =
            disable_suites(&disabled(&["updates"]), DEB822_SRC, "noble", &mut log)
                .unwrap();
        assert_eq!(
            out,
            "Types: deb\nURIs: http://x/\n\
             # cloud-init disable_suites redacted: Suites: noble noble-updates noble-backports\n\
             Suites: noble noble-backports\nComponents: main\n\n\n\
             Types: deb\nURIs: http://sec/\nSuites: noble-security\nComponents: main\n"
        );
    }

    /// Losing every suite disables the whole stanza, with the Suites line put back.
    #[test]
    fn a_deb822_stanza_left_with_no_suites_is_disabled_entirely() {
        let mut log = Logger::silent();
        let out =
            disable_suites(&disabled(&["security"]), DEB822_SRC, "noble", &mut log)
                .unwrap();
        assert_eq!(
            out,
            "Types: deb\nURIs: http://x/\n\
             Suites: noble noble-updates noble-backports\nComponents: main\n\n\n\
             ## Entry disabled by cloud-init, due to disable_suites\n\
             # disabled by cloud-init: Types: deb\n\
             # disabled by cloud-init: URIs: http://sec/\n\
             # disabled by cloud-init: Suites: noble-security\n\
             # disabled by cloud-init: Components: main\n"
        );
    }

    #[test]
    fn an_empty_disable_list_returns_the_source_untouched() {
        let mut log = Logger::silent();
        let out = disable_suites(&[], DEB822_SRC, "noble", &mut log).unwrap();
        assert_eq!(out, DEB822_SRC);
    }

    /// Upstream raises `IndexError` on a line too short to hold a suite. See B77.
    #[test]
    fn a_sources_line_with_no_suite_column_is_an_error() {
        let mut log = Logger::silent();
        for src in ["deb http://x/\n", "deb [arch=amd64] http://x/\n"] {
            let err = disable_suites(&disabled(&["updates"]), src, "noble", &mut log)
                .unwrap_err();
            assert_eq!(err, "list index out of range");
        }
    }

    #[test]
    fn apt_config_paths_come_from_the_dump_or_fall_back_to_defaults() {
        assert_eq!(get_apt_cfg(None), AptPaths::default());
        let dump = "Dir::Etc \"etc/apt\";\nDir::Etc::sourcelist \"sources.list\";\n\
                    Dir::Etc::sourceparts \"sources.list.d\";\n";
        assert_eq!(get_apt_cfg(Some(dump)), AptPaths::default());
        let dump = "Dir::Etc \"custom/apt\";\nDir::Etc::sourcelist \"my.list\";\n";
        assert_eq!(
            get_apt_cfg(Some(dump)),
            AptPaths {
                sourcelist: "/custom/apt/my.list".to_owned(),
                sourceparts: "/custom/apt/sources.list.d/".to_owned(),
            }
        );
    }

    /// Content verified against the installed cloud-init.
    #[test]
    fn proxies_are_written_in_config_order_and_conf_verbatim() {
        let mut log = Logger::silent();
        let cfg =
            obj("proxy: http://a/\nhttps_proxy: https://b/\nconf: 'Foo \"bar\";'\n");
        let actions = plan_apt_config(&cfg, "/p", "/c", &mut |_| false, &mut log);
        assert_eq!(
            actions,
            vec![
                FileAction::Write {
                    path: "/p".to_owned(),
                    content: "Acquire::http::Proxy \"http://a/\";\n\
                              Acquire::https::Proxy \"https://b/\";\n"
                        .to_owned(),
                    mode: None,
                },
                FileAction::Write {
                    path: "/c".to_owned(),
                    content: "Foo \"bar\";".to_owned(),
                    mode: None,
                },
            ]
        );
    }

    #[test]
    fn an_empty_config_removes_the_drop_ins_only_when_they_exist() {
        let mut log = Logger::silent();
        let cfg = Object::new();
        assert!(plan_apt_config(&cfg, "/p", "/c", &mut |_| false, &mut log).is_empty());
        assert_eq!(
            plan_apt_config(&cfg, "/p", "/c", &mut |_| true, &mut log),
            vec![
                FileAction::Remove("/p".to_owned()),
                FileAction::Remove("/c".to_owned()),
            ]
        );
    }

    fn mirrors() -> Object {
        obj("PRIMARY: http://m/\nSECURITY: http://s/\nMIRROR: http://m/\n")
    }

    #[test]
    fn a_deb822_template_lands_in_sources_list_d_and_stubs_the_old_file() {
        let mut log = Logger::silent();
        let actions = plan_sources_list(
            &Object::new(),
            "noble",
            &mirrors(),
            "ubuntu",
            &Object::new(),
            true,
            &AptPaths::default(),
            &mut |name, _| {
                assert_eq!(name, "sources.list.ubuntu.deb822");
                Some("Types: deb\nURIs: $MIRROR\nSuites: $RELEASE\n".to_owned())
            },
            &mut |_| Some("deb http://old/ noble main\n".to_owned()),
            &mut log,
        )
        .unwrap();
        assert_eq!(
            actions,
            vec![
                FileAction::Write {
                    path: "/etc/apt/sources.list.d/ubuntu.sources".to_owned(),
                    content: "Types: deb\nURIs: http://m/\nSuites: noble\n".to_owned(),
                    mode: Some(0o644),
                },
                FileAction::Write {
                    path: "/etc/apt/sources.list".to_owned(),
                    content: UBUNTU_DEFAULT_APT_SOURCES_LIST.to_owned(),
                    mode: None,
                },
            ]
        );
    }

    /// A one-line body goes to sources.list even with the deb822 flag set.
    #[test]
    fn a_one_line_custom_template_overrides_the_deb822_destination() {
        let mut log = Logger::silent();
        let cfg = obj("sources_list: 'deb $MIRROR $RELEASE main'\n");
        let actions = plan_sources_list(
            &cfg,
            "noble",
            &mirrors(),
            "ubuntu",
            &Object::new(),
            true,
            &AptPaths::default(),
            &mut |_, _| panic!("a custom template must not consult the builtins"),
            &mut |_| None,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            actions,
            vec![FileAction::Write {
                path: "/etc/apt/sources.list".to_owned(),
                content: "deb http://m/ noble main".to_owned(),
                mode: Some(0o644),
            }]
        );
    }

    #[test]
    fn a_missing_template_renders_nothing_at_all() {
        let mut log = Logger::silent();
        let actions = plan_sources_list(
            &Object::new(),
            "noble",
            &mirrors(),
            "ubuntu",
            &Object::new(),
            true,
            &AptPaths::default(),
            &mut |_, _| None,
            &mut |_| None,
            &mut log,
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[derive(Default)]
    struct FakeGpg {
        calls: Vec<String>,
        fetched: Option<String>,
        dearmor_fails: bool,
    }

    impl Gpg for FakeGpg {
        fn export_armour(&mut self, _key: &str, _log: &mut Logger) -> Option<String> {
            None
        }

        fn dearmor(&mut self, key: &str) -> Result<Vec<u8>, String> {
            self.calls.push(format!("dearmor {key}"));
            if self.dearmor_fails {
                return Err("gpg said no".to_owned());
            }
            Ok(format!("<binary {key}>").into_bytes())
        }

        fn list_keys(
            &mut self,
            _key_file: &str,
            _human_output: bool,
            _log: &mut Logger,
        ) -> Result<String, String> {
            Ok(String::new())
        }

        fn recv_key(
            &mut self,
            _key: &str,
            _keyserver: &str,
            _log: &mut Logger,
        ) -> Result<(), String> {
            Ok(())
        }

        fn delete_key(&mut self, _key: &str, _log: &mut Logger) {}

        fn getkeybyid(
            &mut self,
            keyid: &str,
            keyserver: &str,
            _log: &mut Logger,
        ) -> Result<Option<String>, String> {
            self.calls.push(format!("getkeybyid {keyid} {keyserver}"));
            Ok(self.fetched.clone())
        }
    }

    fn never_matches(_: &str) -> bool {
        false
    }

    /// Verified against `pathlib.Path(x).stem`. `".."` is the one input where
    /// the two disagree, and no filename can reach it.
    #[test]
    fn the_key_filename_stem_matches_pathlib() {
        for (input, want) in [
            ("primary", "primary"),
            ("a.list", "a"),
            ("a.b.list", "a.b"),
            ("/etc/x/a.list", "a"),
            ("", ""),
            (".hidden", ".hidden"),
            ("a.", "a"),
            ("a/", "a"),
            (".", ""),
            ("/", ""),
        ] {
            assert_eq!(path_stem(input), want, "stem of {input:?}");
        }
    }

    #[test]
    fn a_keyid_source_fetches_the_key_and_signs_the_entry_with_it() {
        let mut log = Logger::silent();
        let mut gpg = FakeGpg {
            fetched: Some("ARMOUR".to_owned()),
            ..FakeGpg::default()
        };
        let srcdict = Value::Object(obj(
            "myrepo:\n  source: 'deb [signed-by=$KEY_FILE] http://r/ $RELEASE main'\n  keyid: ABC\n",
        ));
        let mut params = obj("RELEASE: noble\n");
        let steps = plan_apt_sources(
            &srcdict,
            &mut gpg,
            &mut params,
            &mut never_matches,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            gpg.calls,
            ["getkeybyid ABC keyserver.ubuntu.com", "dearmor ARMOUR"]
        );
        assert_eq!(
            steps,
            vec![
                Step::WriteKey {
                    path: "/etc/apt/cloud-init.gpg.d/myrepo.gpg".to_owned(),
                    content: b"<binary ARMOUR>".to_vec(),
                },
                Step::WriteSource {
                    path: "/etc/apt/sources.list.d/myrepo.list".to_owned(),
                    content: "deb [signed-by=/etc/apt/cloud-init.gpg.d/myrepo.gpg] \
                              http://r/ noble main\n"
                        .to_owned(),
                    append: true,
                },
                Step::UpdatePackageSources,
            ]
        );
    }

    #[test]
    fn an_explicit_keyserver_is_used_and_a_plain_key_is_not_hardened() {
        let mut log = Logger::silent();
        let mut gpg = FakeGpg::default();
        let srcdict = Value::Object(obj(
            "myrepo.list:\n  source: 'deb http://r/ noble main'\n  keyid: ABC\n  keyserver: pgp.mit.edu\n  append: false\n",
        ));
        let steps = plan_apt_sources(
            &srcdict,
            &mut gpg,
            &mut Object::new(),
            &mut never_matches,
            &mut log,
        )
        .unwrap();
        assert_eq!(gpg.calls, ["getkeybyid ABC pgp.mit.edu", "dearmor "]);
        assert_eq!(
            steps,
            vec![
                // Not `$KEY_FILE`, so the key lands in the trusted dir, and the
                // `.list` suffix is stripped from the keyring name.
                Step::WriteKey {
                    path: "/etc/apt/trusted.gpg.d/myrepo.gpg".to_owned(),
                    content: b"<binary >".to_vec(),
                },
                Step::WriteSource {
                    path: "/etc/apt/sources.list.d/myrepo.list".to_owned(),
                    content: "deb http://r/ noble main\n".to_owned(),
                    append: false,
                },
                Step::UpdatePackageSources,
            ]
        );
    }

    #[test]
    fn a_ppa_style_source_goes_to_add_apt_repository_instead_of_a_file() {
        let mut log = Logger::silent();
        let mut gpg = FakeGpg::default();
        let srcdict =
            Value::Object(obj("ppa:\n  source: 'ppa:cloud-init-dev/daily'\n"));
        let steps = plan_apt_sources(
            &srcdict,
            &mut gpg,
            &mut Object::new(),
            &mut |source| {
                regex::Regex::new(ADD_APT_REPO_MATCH)
                    .unwrap()
                    .is_match(source)
            },
            &mut log,
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::AddAptRepository {
                    source: "ppa:cloud-init-dev/daily".to_owned(),
                },
                Step::UpdatePackageSources,
            ]
        );
    }

    #[test]
    fn a_dearmour_failure_leaves_dev_null_in_the_rendered_source() {
        let mut log = Logger::silent();
        let mut gpg = FakeGpg {
            fetched: Some("ARMOUR".to_owned()),
            dearmor_fails: true,
            ..FakeGpg::default()
        };
        let srcdict = Value::Object(obj(
            "myrepo:\n  source: 'deb [signed-by=$KEY_FILE] http://r/ noble main'\n  keyid: ABC\n",
        ));
        let steps = plan_apt_sources(
            &srcdict,
            &mut gpg,
            &mut Object::new(),
            &mut never_matches,
            &mut log,
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![
                Step::WriteSource {
                    path: "/etc/apt/sources.list.d/myrepo.list".to_owned(),
                    content: "deb [signed-by=/dev/null] http://r/ noble main\n"
                        .to_owned(),
                    append: true,
                },
                Step::UpdatePackageSources,
            ]
        );
    }

    #[test]
    fn mirror_keys_are_published_under_primary_key_and_security_key() {
        let mut log = Logger::silent();
        let mut gpg = FakeGpg::default();
        let mut cfg = obj(
            "primary:\n  - arches: [default]\n    uri: http://m/\n    key: RAW\nsecurity:\n  - arches: [default]\n    uri: http://s/\n",
        );
        let mut steps = Vec::new();
        let keys = add_mirror_keys(&mut cfg, &mut gpg, &mut steps, &mut log).unwrap();
        assert_eq!(
            steps,
            vec![Step::WriteKey {
                path: "/etc/apt/trusted.gpg.d/primary.gpg".to_owned(),
                content: b"<binary RAW>".to_vec(),
            }]
        );
        assert_eq!(
            keys,
            obj("primary_key: /etc/apt/trusted.gpg.d/primary.gpg\n")
        );
    }

    #[test]
    fn only_the_absent_dependencies_are_requested_and_they_come_out_sorted() {
        let cfg = obj(
            "primary:\n  - arches: [default]\n    keyid: ABC\nsources:\n  ppa:\n    source: 'ppa:x/y'\n",
        );
        let mut matches = |source: &str| source.starts_with("ppa:");
        assert_eq!(
            ensure_dependencies(&cfg, &mut matches, &mut |_| false),
            ["gnupg", "software-properties-common"]
        );
        assert_eq!(
            ensure_dependencies(&cfg, &mut matches, &mut |command| command == "gpg"),
            ["software-properties-common"]
        );
        assert!(ensure_dependencies(&cfg, &mut matches, &mut |_| true).is_empty());
    }

    /// `preserve_sources_list` takes the mirror keys out of the picture, but
    /// not the `sources` ones.
    #[test]
    fn preserving_the_sources_list_drops_only_the_mirror_key_dependency() {
        let cfg = obj("preserve_sources_list: true\nprimary:\n  - arches: [default]\n    keyid: ABC\n");
        assert!(
            ensure_dependencies(&cfg, &mut never_matches, &mut |_| false).is_empty()
        );
    }

    /// Output and expectation both taken from `util.get_installed_packages`.
    #[test]
    fn only_installed_and_held_packages_are_read_off_dpkg_query() {
        let stdout = "Desired=Unknown/Install\n\
                      ||/ Name           Version      Arch  Description\n\
                      +++-==============-============-=====-=========\n\
                      ii  cloud-init     26.1         all   init\n\
                      hi  held-pkg:amd64 1.0          amd64 held\n\
                      rc  removed-pkg    1.0          all   removed\n\
                      un  unknown\n";
        assert_eq!(installed_packages(stdout), ["cloud-init", "held-pkg"]);
    }

    #[test]
    fn debconf_selections_are_joined_in_key_order_and_newline_terminated() {
        let mut log = Logger::silent();
        let cfg = obj(
            "debconf_selections:\n  set2: 'pkg pkg/value string bar'\n  set1: 'cloud-init cloud-init/datasources multiselect MAAS'\n",
        );
        let plan = plan_debconf_selections(&cfg, &[], &mut log)
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(plan.selections).unwrap(),
            "cloud-init cloud-init/datasources multiselect MAAS\n\
             pkg pkg/value string bar\n"
        );
        assert!(plan.reconfigure.is_empty());
        assert!(plan.unhandled.is_empty());
    }

    #[test]
    fn only_preseeded_packages_that_are_installed_are_reconfigured() {
        let mut log = Logger::silent();
        let cfg = obj(
            "debconf_selections:\n  a: |\n    # a comment\n    cloud-init cloud-init/datasources multiselect MAAS\n    pkg:i386 pkg/value string bar\n    absent absent/x string y\n",
        );
        let installed = ["cloud-init".to_owned(), "pkg".to_owned()];
        let plan = plan_debconf_selections(&cfg, &installed, &mut log)
            .unwrap()
            .unwrap();
        assert_eq!(plan.reconfigure, ["cloud-init"]);
        assert_eq!(plan.unhandled, ["pkg"]);
    }

    #[test]
    fn an_unset_debconf_block_plans_nothing_and_a_scalar_one_is_an_error() {
        let mut log = Logger::silent();
        assert!(plan_debconf_selections(&Object::new(), &[], &mut log)
            .unwrap()
            .is_none());
        assert_eq!(
            plan_debconf_selections(&obj("debconf_selections: nope\n"), &[], &mut log),
            Err("'str' object has no attribute 'keys'".to_owned())
        );
    }

    #[test]
    fn apt_list_renames_skip_mirrors_that_did_not_move() {
        let new_mirrors = obj(
            "PRIMARY: http://azure/ubuntu/\nSECURITY: http://security.ubuntu.com/ubuntu/\n",
        );
        assert_eq!(
            plan_apt_list_renames(&new_mirrors, "amd64", "/var/lib/apt/lists").unwrap(),
            vec![ListRename {
                from_prefix: "/var/lib/apt/lists/archive.ubuntu.com_ubuntu".to_owned(),
                to_prefix: "/var/lib/apt/lists/azure_ubuntu".to_owned(),
            }]
        );
    }
}
