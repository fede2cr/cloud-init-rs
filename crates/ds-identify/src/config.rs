//! `ds-identify.cfg`, the kernel command line, `cloud.cfg*` grepping, and the
//! policy string.

use std::path::Path;

use crate::glob;
use crate::log::Log;
use crate::paths::Paths;
use crate::shell::{glob_match, split_ifs, split_words, trim, unquote};

pub const DEFAULT_POLICY: &str = "search,found=all,maybe=none,notfound=disabled";

/// The 30-entry builtin list. Must match cloud-init's own builtin.
pub const DSLIST_DEFAULT: &str = "MAAS ConfigDrive NoCloud AltCloud Azure Bigstep \
CloudSigma CloudStack DigitalOcean Vultr AliYun Ec2 GCE OpenNebula OpenStack \
VMware OVF SmartOS Scaleway Hetzner IBMCloud Oracle Exoscale RbxCloud UpCloud \
LXD NWCS Akamai WSL CloudCIX";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Enabled,
    Disabled,
    Search,
    Report,
}

impl Mode {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            "search" => Some(Self::Search),
            "report" => Some(Self::Report),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Search => "search",
            Self::Report => "report",
        }
    }
}

/// The five `_rc_*` variables `parse_policy` sets.
#[derive(Debug, Clone)]
pub struct Policy {
    pub mode: String,
    pub report: String,
    pub found: String,
    pub maybe: String,
    pub notfound: String,
}

/// `parse_policy(policy)`: parse `policy`, defaulting each unset field from
/// [`DEFAULT_POLICY`].
///
/// The `DI_UNAME_MACHINE` switch upstream picks between `DI_DEFAULT_POLICY` and
/// `DI_DEFAULT_POLICY_NO_DMI`, which are the same string, so the port does not
/// reproduce the branch.
#[must_use]
pub fn parse_policy(policy: &str) -> Policy {
    let defaults = parse_policy_tokens(DEFAULT_POLICY, &Policy::empty());
    parse_policy_tokens(policy, &defaults)
}

impl Policy {
    fn empty() -> Self {
        Self {
            mode: String::new(),
            report: String::new(),
            found: String::new(),
            maybe: String::new(),
            notfound: String::new(),
        }
    }
}

fn parse_policy_tokens(policy: &str, defaults: &Policy) -> Policy {
    let (mut mode, mut found, mut maybe, mut notfound) = (None, None, None, None);
    for token in split_ifs(policy, ',') {
        let value = match token.split_once('=') {
            Some((_, v)) => v,
            None => token,
        };
        if let Some(parsed) = Mode::parse(token) {
            mode = Some(parsed.as_str().to_owned());
            continue;
        }
        match token.split_once('=') {
            Some(("found", "all" | "first")) => found = Some(value.to_owned()),
            Some(("maybe", "all" | "none")) => maybe = Some(value.to_owned()),
            Some(("notfound", "enabled" | "disabled")) => {
                notfound = Some(value.to_owned());
            }
            Some(("found", _)) => {
                parse_warn("found", value, &defaults.found);
                found = Some(defaults.found.clone());
            }
            Some(("maybe", _)) => {
                parse_warn("maybe", value, &defaults.maybe);
                maybe = Some(defaults.maybe.clone());
            }
            Some(("notfound", _)) => {
                parse_warn("notfound", value, &defaults.notfound);
                notfound = Some(defaults.notfound.clone());
            }
            _ => {}
        }
    }
    Policy {
        // `report` is never assigned by the loop upstream, so it is always
        // "false". Kept because `_print_info` and the debug line print it.
        report: if defaults.report.is_empty() {
            "false".to_owned()
        } else {
            defaults.report.clone()
        },
        mode: mode.unwrap_or_else(|| defaults.mode.clone()),
        found: found.unwrap_or_else(|| defaults.found.clone()),
        maybe: maybe.unwrap_or_else(|| defaults.maybe.clone()),
        notfound: notfound.unwrap_or_else(|| defaults.notfound.clone()),
    }
}

fn parse_warn(key: &str, value: &str, default: &str) {
    eprintln!("WARN: invalid value '{value}' for key '{key}'. Using {key}={default}.");
}

/// `_read_config` with no key: harvest `datasource` and `policy`.
#[must_use]
pub fn read_config_file(text: &str) -> (String, String) {
    let (mut dsname, mut policy) = (String::new(), String::new());
    for entry in config_entries(text) {
        match entry.0.as_str() {
            "datasource" => dsname = entry.1,
            "policy" => policy = entry.1,
            _ => {}
        }
    }
    (dsname, policy)
}

/// `_read_config <keyname>`: the first matching key wins.
#[must_use]
pub fn read_config_key(text: &str, keyname: &str) -> Option<String> {
    config_entries(text)
        .into_iter()
        .find(|(key, _)| key == keyname)
        .map(|(_, value)| value)
}

fn config_entries(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or_default();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        out.push((trim(key).to_owned(), unquote(trim(value)).to_owned()));
    }
    out
}

/// What `check_config` returns: the value and the file it came from.
#[derive(Debug, Clone)]
pub struct ConfigMatch {
    pub value: String,
    pub fname: String,
}

/// `check_config(key [, globs])`.
///
/// Deliberately not a YAML parse: upstream greps, so a key nested three levels
/// deep under an unrelated mapping matches just as well as a top-level one, and
/// the *last* match across all files wins regardless of hierarchy. Reproducing
/// that is the point -- a port that parsed YAML properly would disagree with
/// the running system.
#[must_use]
pub fn check_config(paths: &Paths, key: &str, globs: &[String]) -> Option<ConfigMatch> {
    let patterns: Vec<String> = if globs.is_empty() {
        vec![
            paths.etc_ci_cfg.to_string_lossy().into_owned(),
            format!("{}/*.cfg", paths.etc_ci_cfg_d.display()),
        ]
    } else {
        globs.to_vec()
    };
    let joined = patterns.join(" ");
    let files: Vec<String> = patterns
        .iter()
        .flat_map(|p| split_words(p))
        .flat_map(glob::expand)
        .collect();
    let first = files.first()?;
    if *first == joined && !Path::new(first).is_file() {
        return None;
    }

    let single = files.len() == 1;
    let mut last: Option<ConfigMatch> = None;
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for raw in text.lines() {
            if !line_matches_key(raw, key) {
                continue;
            }
            // grep prefixes each line with `filename:` when it was handed more
            // than one file. Upstream strips the `# comment` first and only
            // then splits the prefix back off, so a line that is entirely a
            // comment leaves nothing behind and is skipped.
            let output = if single {
                raw.to_owned()
            } else {
                format!("{file}:{raw}")
            };
            let output = output.split('#').next().unwrap_or_default();
            let (fname, line) = if single {
                (file.clone(), output)
            } else {
                let fname = output.split(':').next().unwrap_or_default().to_owned();
                let Some(rest) = output.strip_prefix(&format!("{fname}:")) else {
                    continue;
                };
                (fname, rest)
            };
            if line.is_empty() {
                continue;
            }
            let value = match line.split_once(": ") {
                Some((_, rest)) => rest.to_owned(),
                None => line.to_owned(),
            };
            last = Some(ConfigMatch { value, fname });
        }
    }
    last
}

/// The BRE `key["\']*[[:space:]]*:`.
///
/// The bracket expression really does contain a backslash: the shell strips one
/// level from `"$key[\"\']*..."`, leaving `["\']` for grep.
fn line_matches_key(line: &str, key: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let key_chars: Vec<char> = key.chars().collect();
    for start in 0..chars.len() {
        if chars.get(start..start + key_chars.len()) != Some(key_chars.as_slice()) {
            continue;
        }
        let mut i = start + key_chars.len();
        while matches!(chars.get(i), Some('"' | '\\' | '\'')) {
            i += 1;
        }
        while matches!(chars.get(i), Some(c) if crate::shell::is_space(*c)) {
            i += 1;
        }
        if chars.get(i) == Some(&':') {
            return true;
        }
    }
    false
}

/// `get_value(key, value)`: everything after the final colon, trimmed,
/// non-empty.
#[must_use]
pub fn get_value(log: &mut Log, key: &str, value: &str) -> Option<String> {
    let tail = match value.rsplit_once(':') {
        Some((_, rest)) => rest,
        None => value,
    };
    let trimmed = trim(tail);
    if trimmed.is_empty() {
        log.debug(1, &format!("key {key} didn't have a valid value"));
        return None;
    }
    Some(trimmed.to_owned())
}

/// `get_single_line_flow_sequence(key, value)`.
///
/// Upstream strips the brackets into a scratch variable, restores `_RET` from
/// `get_value`, and then length-checks the restored value -- so the bracket
/// stripping is discarded and the function succeeds exactly when `get_value`
/// does. Reproduced rather than corrected, because the caller feeds `_RET`
/// (brackets and all) to `parse_yaml_array`, which strips them again.
#[must_use]
pub fn get_single_line_flow_sequence(
    log: &mut Log,
    key: &str,
    value: &str,
) -> Option<String> {
    get_value(log, key, value)
}

/// `parse_yaml_array`: `[ a, "b" ]` becomes `a b`.
#[must_use]
pub fn parse_yaml_array(value: &str) -> String {
    let value = trim(value);
    let value = value.strip_prefix('[').unwrap_or(value);
    let value = value.strip_suffix(']').unwrap_or(value);
    split_ifs(value, ',')
        .into_iter()
        .map(|token| unquote(trim(token)).to_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `read_datasource_list`.
#[must_use]
pub fn read_datasource_list(
    log: &mut Log,
    paths: &Paths,
    dsname: &str,
    kernel_cmdline: &str,
) -> String {
    let mut dslist = String::new();
    // A named datasource short-circuits the config parse. Upstream logs about
    // it in `_main`, not here.
    if !dsname.is_empty() {
        dsname.clone_into(&mut dslist);
    }

    // `ds=nocloud;s=...` style is handled by DI_DSNAME; this is the
    // `cc:{'datasource_list': [ ... ]}` form.
    if glob_match("*cc:*datasource_list*", kernel_cmdline) {
        let tail = match kernel_cmdline.rsplit_once("datasource_list") {
            Some((_, rest)) => rest,
            None => kernel_cmdline,
        };
        let tail = match tail.split_once(']') {
            Some((head, _)) => head,
            None => tail,
        };
        let tail = match tail.rsplit_once('[') {
            Some((_, rest)) => rest,
            None => tail,
        };
        // Note this overwrites whatever DI_DSNAME set, and does so silently.
        dslist = parse_yaml_array(tail);
    }

    if dslist.is_empty() {
        if let Some(found) = check_config(paths, "datasource_list", &[]) {
            if let Some(value) =
                get_single_line_flow_sequence(log, "datasource_list", &found.value)
            {
                log.debug(1, &format!("{} set datasource_list: {value}", found.fname));
                dslist = parse_yaml_array(&value);
            }
        }
    }

    if dslist.is_empty() {
        DSLIST_DEFAULT.clone_into(&mut dslist);
        log.warn(&format!(
            "no datasource_list found, using default: {dslist}"
        ));
    }
    dslist
}

/// `read_config`: the config file, then the kernel command line, then the
/// policy.
#[derive(Debug, Clone)]
pub struct ConfigResult {
    pub dsname: String,
    pub policy: Policy,
}

#[must_use]
pub fn read_config(log: &mut Log, paths: &Paths, kernel_cmdline: &str) -> ConfigResult {
    let (mut dsname, mut policy_str) = (String::new(), String::new());
    if paths.di_config.is_file() {
        if let Ok(text) = std::fs::read_to_string(&paths.di_config) {
            let (d, p) = read_config_file(&text);
            dsname = d;
            policy_str = p;
        }
    } else if paths.di_config.symlink_metadata().is_ok() {
        log.error(&format!(
            "{} exists but is not a file!",
            paths.di_config.display()
        ));
    }

    for token in split_words(kernel_cmdline) {
        let (key, value) = match token.split_once('=') {
            Some((key, value)) => (key, value),
            // A token with no '=' yields key == value == token upstream, which
            // means a bare `ds` on the command line sets DI_DSNAME=ds.
            None => (token, token),
        };
        let value = value.split(';').next().unwrap_or_default();
        match key {
            "ds" | "ci.ds" | "ci.datasource" => value.clone_into(&mut dsname),
            "ci.di.policy" => value.clone_into(&mut policy_str),
            _ => {}
        }
    }

    let policy = parse_policy(&policy_str);
    log.debug(
        1,
        &format!(
            "policy loaded: mode={} report={} found={} maybe={} notfound={}",
            policy.mode, policy.report, policy.found, policy.maybe, policy.notfound
        ),
    );
    ConfigResult { dsname, policy }
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
    fn the_default_policy_round_trips() {
        let policy = parse_policy("");
        assert_eq!(policy.mode, "search");
        assert_eq!(policy.found, "all");
        assert_eq!(policy.maybe, "none");
        assert_eq!(policy.notfound, "disabled");
        assert_eq!(policy.report, "false");
    }

    #[test]
    fn an_explicit_policy_overrides_field_by_field() {
        let policy = parse_policy("enabled,found=first");
        assert_eq!(policy.mode, "enabled");
        assert_eq!(policy.found, "first");
        assert_eq!(policy.maybe, "none");
    }

    #[test]
    fn an_invalid_value_falls_back_to_the_default() {
        let policy = parse_policy("search,found=bogus");
        assert_eq!(policy.found, "all");
    }

    #[test]
    fn an_unrecognised_token_is_ignored() {
        let policy = parse_policy("search,whatever");
        assert_eq!(policy.mode, "search");
    }

    #[test]
    fn config_entries_split_on_the_first_colon_only() {
        let (dsname, policy) =
            read_config_file("datasource: Ec2\npolicy: search,found=first\n");
        assert_eq!(dsname, "Ec2");
        assert_eq!(policy, "search,found=first");
    }

    #[test]
    fn a_comment_truncates_the_line() {
        assert_eq!(read_config_file("datasource: Ec2 # nope\n").0, "Ec2");
        // The '#' is stripped before the colon is looked for, so a fully
        // commented line has no key at all.
        assert_eq!(read_config_file("# datasource: Ec2\n").0, "");
    }

    #[test]
    fn quotes_are_stripped() {
        assert_eq!(read_config_file("datasource: \"Ec2\"\n").0, "Ec2");
        assert_eq!(read_config_file("datasource: 'Ec2'\n").0, "Ec2");
    }

    #[test]
    fn the_key_regex_allows_quotes_and_spaces() {
        assert!(line_matches_key(
            "datasource_list: [ Ec2 ]",
            "datasource_list"
        ));
        assert!(line_matches_key(
            "\"datasource_list\"  : [ Ec2 ]",
            "datasource_list"
        ));
        assert!(line_matches_key(
            "  datasource_list\t: [ Ec2 ]",
            "datasource_list"
        ));
        assert!(!line_matches_key(
            "datasource_list = [ Ec2 ]",
            "datasource_list"
        ));
        // Grep is not anchored, so a nested key matches too.
        assert!(line_matches_key(
            "  foo: {datasource_list: []}",
            "datasource_list"
        ));
    }

    #[test]
    fn yaml_arrays_are_split_on_commas() {
        assert_eq!(parse_yaml_array("[ Ec2, None ]"), "Ec2 None");
        assert_eq!(parse_yaml_array("['Ec2','None']"), "Ec2 None");
        assert_eq!(parse_yaml_array("[]"), "");
    }
}
