//! Port of `cloudinit/ssh_util.py`: parsing an `authorized_keys` file, merging
//! new keys into it, reading the handful of `sshd_config` keywords cloud-init
//! cares about, and installing the result.
//!
//! Two modules need this and neither is a natural owner. `cc_users_groups`
//! reaches it through `Distro.create_user`, which installs a user's keys and —
//! for `ssh_redirect_user` — installs the *default* user's keys under a forced
//! command that refuses the login. `cc_ssh` reaches it directly. So it lives
//! where it does upstream: on its own.
//!
//! The crate is split along the line that decides how it can be tested. This
//! module is everything that is a function of text, and is checked against
//! upstream byte for byte. [`install`] is everything that stats, creates and
//! writes, and is checked against a fixture tree; every entry point there takes
//! a `root` so that a test cannot reach the machine it runs on.
//!
//! The parser is modelled on OpenSSH's `auth2-pubkey.c`, via upstream's
//! transcription of it. Its most important property is that an unparseable
//! line is *kept verbatim* rather than dropped — an `authorized_keys` file
//! cloud-init does not understand must survive cloud-init touching it.

use std::fmt;

pub mod config;
pub mod install;

pub use config::{config_map, parse_config_lines, SshdConfigLine};
pub use install::{
    append_ssh_config, extract_authorized_keys, setup_user_keys, update_ssh_config,
    update_ssh_config_lines, users_ssh_info, would_update_ssh_config, DEF_SSHD_CFG,
};

/// Key types cloud-init will accept, filtered from OpenSSH's `sshkey.c` with
/// the signature-only entries removed.
///
/// A line whose first token is not in here is not treated as a key at all — it
/// is re-parsed as `options keytype base64`, and failing that kept as opaque
/// text.
pub const VALID_KEY_TYPES: [&str; 19] = [
    "rsa",
    "ecdsa",
    "ed25519",
    "ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384-cert-v01@openssh.com",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521-cert-v01@openssh.com",
    "ecdsa-sha2-nistp521",
    "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "sk-ssh-ed25519-cert-v01@openssh.com",
    "sk-ssh-ed25519@openssh.com",
    "ssh-ed25519-cert-v01@openssh.com",
    "ssh-ed25519",
    "ssh-rsa-cert-v01@openssh.com",
    "ssh-rsa",
    "ssh-xmss-cert-v01@openssh.com",
    "ssh-xmss@openssh.com",
];

/// The exit status the redirect banner ends with.
const DISABLE_USER_SSH_EXIT: u32 = 142;

/// The options prefix that turns a key into a refusal.
///
/// `$USER` and `$DISABLE_USER` are placeholders the caller substitutes: the
/// key belongs to the default account, is installed under the *other*
/// account's name, and the forced command prints "log in as the other one" and
/// exits. Nothing about it is a real lock — it works only because a forced
/// command runs instead of the shell.
pub fn disable_user_opts() -> String {
    format!(
        "no-port-forwarding,no-agent-forwarding,no-X11-forwarding,\
         command=\"echo 'Please login as the user \\\"$USER\\\" rather than the \
         user \\\"$DISABLE_USER\\\".';echo;sleep 10;exit {DISABLE_USER_SSH_EXIT}\""
    )
}

/// One line of an `authorized_keys` file.
///
/// The empty string stands in for upstream's `None` throughout, because every
/// test upstream makes on these fields is a truthiness test and an empty
/// `keytype` is as invalid as a missing one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthKeyLine {
    /// The line as it arrived, returned unchanged when nothing parsed.
    pub source: String,
    pub keytype: String,
    pub base64: String,
    pub comment: String,
    pub options: String,
}

impl AuthKeyLine {
    /// An opaque line: a comment, a blank, or something unrecognised.
    #[must_use]
    pub fn opaque(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            ..Self::default()
        }
    }

    /// Whether this line is a key, and so eligible to be matched against and
    /// replaced by a new one.
    #[must_use]
    pub fn valid(&self) -> bool {
        !self.base64.is_empty() && !self.keytype.is_empty()
    }
}

impl fmt::Display for AuthKeyLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = [&self.options, &self.keytype, &self.base64, &self.comment];
        let mut toks = parts.iter().filter(|part| !part.is_empty()).peekable();
        if toks.peek().is_none() {
            return write!(f, "{}", self.source);
        }
        let mut first = true;
        for tok in toks {
            if !first {
                write!(f, " ")?;
            }
            write!(f, "{tok}")?;
            first = false;
        }
        Ok(())
    }
}

/// `AuthKeyLineParser.parse`.
///
/// `options`, when non-empty, overrides whatever the line carried. That is how
/// `ssh_redirect_user` forces its banner onto a key that may already have
/// options of its own.
#[must_use]
pub fn parse_auth_key_line(src_line: &str, options: &str) -> AuthKeyLine {
    let line = src_line.trim_end_matches(['\r', '\n']);
    if line.starts_with('#') || ci_core::pystr::strip(line).is_empty() {
        return AuthKeyLine::opaque(src_line);
    }
    let ent = ci_core::pystr::strip(line);

    if let Some((keytype, base64, comment)) = parse_ssh_key(ent) {
        return AuthKeyLine {
            source: src_line.to_owned(),
            keytype,
            base64,
            comment,
            options: options.to_owned(),
        };
    }

    let (keyopts, remain) = extract_options(ent);
    let options = if options.is_empty() {
        &keyopts
    } else {
        options
    };
    let Some((keytype, base64, comment)) = parse_ssh_key(remain) else {
        // Note that the options found above are discarded: a line that does
        // not parse keeps nothing but its own text.
        return AuthKeyLine::opaque(src_line);
    };
    AuthKeyLine {
        source: src_line.to_owned(),
        keytype,
        base64,
        comment,
        options: options.to_owned(),
    }
}

/// `parse_ssh_key`: three fields at most, of which the third is optional.
fn parse_ssh_key(ent: &str) -> Option<(String, String, String)> {
    let toks = ci_core::pystr::split_whitespace_n(ent, 2);
    let (keytype, base64) = (toks.first()?, toks.get(1)?);
    if !VALID_KEY_TYPES.contains(keytype) {
        return None;
    }
    Some((
        (*keytype).to_owned(),
        (*base64).to_owned(),
        toks.get(2).unwrap_or(&"").to_owned().to_owned(),
    ))
}

/// `AuthKeyLineParser._extract_options`.
///
/// Scans to the first unquoted space or tab. Transcribed with its two quirks
/// intact: a backslash only escapes a double quote, and the character *before*
/// the end of the string is never examined for quoting, so an option list
/// ending in an unbalanced quote consumes the whole line.
fn extract_options(ent: &str) -> (String, &str) {
    let chars: Vec<char> = ent.chars().collect();
    let mut quoted = false;
    let mut i = 0;
    while i < chars.len() {
        let Some(&curc) = chars.get(i) else { break };
        if !quoted && (curc == ' ' || curc == '\t') {
            break;
        }
        let Some(&nextc) = chars.get(i + 1) else {
            i += 1;
            break;
        };
        if curc == '\\' && nextc == '"' {
            i += 1;
        } else if curc == '"' {
            quoted = !quoted;
        }
        i += 1;
    }
    let split = chars.iter().take(i).map(|c| c.len_utf8()).sum();
    let options = ent.get(..split).unwrap_or("").to_owned();
    let remain = ent
        .get(split..)
        .unwrap_or("")
        .trim_start_matches(ci_core::pystr::is_space);
    (options, remain)
}

/// `parse_authorized_keys`, over content already read from disk.
#[must_use]
pub fn parse_authorized_keys(content: &str) -> Vec<AuthKeyLine> {
    ci_core::pystr::split_lines(content)
        .into_iter()
        .map(|line| parse_auth_key_line(line, ""))
        .collect()
}

/// `update_authorized_keys`: merge `keys` into `old_entries` by base64.
///
/// A key already present is *replaced in place* rather than appended, so
/// re-running keeps the file the same length and preserves the position of
/// everything the user put there by hand. Lines that are not keys are never
/// touched.
#[must_use]
pub fn update_authorized_keys(
    old_entries: &[AuthKeyLine],
    keys: &[AuthKeyLine],
) -> String {
    let mut entries = old_entries.to_vec();
    let mut to_add: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, k)| k.valid())
        .map(|(i, _)| i)
        .collect();

    for ent in &mut entries {
        if !ent.valid() {
            continue;
        }
        for (i, k) in keys.iter().enumerate() {
            if k.base64 == ent.base64 {
                ent.clone_from(k);
                to_add.retain(|&j| j != i);
            }
        }
    }
    for i in to_add {
        if let Some(k) = keys.get(i) {
            entries.push(k.clone());
        }
    }

    let mut lines: Vec<String> = entries.iter().map(ToString::to_string).collect();
    lines.push(String::new());
    lines.join("\n")
}

/// `render_authorizedkeysfile_paths`: expand sshd's `%h`, `%u` and `%%` tokens.
///
/// The macros are applied in that order rather than in one pass, which is
/// upstream's own behaviour and is visible: `%%h` expands the `%h` first and
/// yields `%<homedir>`, not `%h`.
#[must_use]
pub fn render_authorizedkeysfile_paths(
    value: &str,
    homedir: &str,
    username: &str,
) -> Vec<String> {
    let value = if value.is_empty() {
        "%h/.ssh/authorized_keys"
    } else {
        value
    };
    ci_core::pystr::split_whitespace_n(value, usize::MAX)
        .into_iter()
        .map(|path| {
            let path = path
                .replace("%h", homedir)
                .replace("%u", username)
                .replace("%%", "%");
            if path.starts_with('/') {
                path
            } else {
                join(homedir, &path)
            }
        })
        .collect()
}

/// `os.path.join` for the one shape this module needs.
fn join(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
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

    const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2E";

    #[test]
    fn a_plain_key_parses_into_three_fields() {
        let line = parse_auth_key_line("ssh-rsa AAAAB3NzaC1yc2E alice@host", "");
        assert_eq!(line.keytype, "ssh-rsa");
        assert_eq!(line.base64, "AAAAB3NzaC1yc2E");
        assert_eq!(line.comment, "alice@host");
        assert!(line.options.is_empty());
        assert!(line.valid());
    }

    #[test]
    fn a_comment_is_kept_verbatim_and_is_not_a_key() {
        for text in ["# a comment", "", "   ", "not a key at all"] {
            let line = parse_auth_key_line(text, "");
            assert!(!line.valid(), "{text:?}");
            assert_eq!(line.to_string(), text);
        }
    }

    #[test]
    fn an_unknown_keytype_is_retried_as_options() {
        let line = parse_auth_key_line("no-pty,no-X11-forwarding ssh-rsa AAAA bob", "");
        assert_eq!(line.options, "no-pty,no-X11-forwarding");
        assert_eq!(line.keytype, "ssh-rsa");
        assert_eq!(line.comment, "bob");
    }

    #[test]
    fn a_space_inside_quotes_does_not_end_the_options() {
        let line = parse_auth_key_line(r#"command="echo hi there" ssh-rsa AAAA"#, "");
        assert_eq!(line.options, r#"command="echo hi there""#);
        assert_eq!(line.base64, "AAAA");
    }

    #[test]
    fn an_explicit_options_argument_overrides_the_lines_own() {
        let line = parse_auth_key_line("no-pty ssh-rsa AAAA", "forced");
        assert_eq!(line.options, "forced");
    }

    #[test]
    fn a_matching_key_is_replaced_in_place_not_appended() {
        let old = parse_authorized_keys(&format!(
            "# mine\n{RSA} old-comment\nssh-rsa BBBB other\n"
        ));
        let new = vec![parse_auth_key_line(&format!("{RSA} new-comment"), "")];
        assert_eq!(
            update_authorized_keys(&old, &new),
            format!("# mine\n{RSA} new-comment\nssh-rsa BBBB other\n")
        );
    }

    #[test]
    fn an_unmatched_key_is_appended_and_the_file_ends_with_a_newline() {
        let old = parse_authorized_keys("# mine\n");
        let new = vec![parse_auth_key_line(RSA, "")];
        assert_eq!(
            update_authorized_keys(&old, &new),
            format!("# mine\n{RSA}\n")
        );
    }

    #[test]
    fn an_invalid_new_key_is_dropped_rather_than_written() {
        let old = parse_authorized_keys("");
        let new = vec![parse_auth_key_line("garbage", "")];
        // No entries at all, so the trailing "" is the whole file.
        assert_eq!(update_authorized_keys(&old, &new), "");
    }

    #[test]
    fn the_default_path_is_used_when_sshd_names_none() {
        assert_eq!(
            render_authorizedkeysfile_paths("", "/home/alice", "alice"),
            ["/home/alice/.ssh/authorized_keys"]
        );
    }

    #[test]
    fn tokens_expand_in_upstreams_order() {
        assert_eq!(
            render_authorizedkeysfile_paths(
                "%h/.ssh/ak %u.keys /etc/ssh/%u",
                "/home/a",
                "alice"
            ),
            ["/home/a/.ssh/ak", "/home/a/alice.keys", "/etc/ssh/alice"]
        );
        // `%h` is substituted before `%%`, so this is not `%h`.
        assert_eq!(
            render_authorizedkeysfile_paths("/x/%%h", "/home/a", "alice"),
            ["/x/%/home/a"]
        );
    }
}
