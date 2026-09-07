//! Port of `cc_ssh_authkey_fingerprints.py`: print each user's authorized-key
//! fingerprints on the console, in a box.
//!
//! Everything here is formatting, and the formatting is the point: the two
//! centring rules below are *different*, and getting either wrong shifts every
//! line of the table by a space.

use ci_config::{Object, Value};
use ci_log::Logger;

use super::Args;

const SOURCE: &str = "cc_ssh_authkey_fingerprints.py";

/// `prefix` as `_pprint_key_entries` defaults it.
const PREFIX: &str = "ci-info: ";

/// One authorized-keys entry, as `ssh_util.parse_authorized_keys` leaves it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub keytype: String,
    pub base64: String,
    pub comment: String,
    pub options: String,
}

/// What the module asks of the machine.
pub trait Host {
    /// `ssh_util.extract_authorized_keys(user)`: the file it settled on and
    /// what it found in it.
    fn authorized_keys(&mut self, user: &str) -> (String, Vec<Entry>);

    /// `log_util.multi_log(text, stderr=False, console=True)`.
    fn multi_log(&mut self, text: &str);
}

/// `_split_hash` then `":".join(...)`: the hex digest in byte-sized pieces.
#[must_use]
pub fn split_hash(digest: &str) -> String {
    digest
        .as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// `_gen_fingerprint`.
///
/// `"?"` is upstream's answer both to base64 that will not decode and to a
/// hash name `hashlib` does not know.
#[must_use]
pub fn gen_fingerprint(b64_text: &str, hash_meth: &str) -> String {
    if b64_text.is_empty() {
        return String::new();
    }
    let Some(blob) = py_b64decode(b64_text) else {
        return "?".to_owned();
    };
    ci_core::sha::hexdigest(hash_meth, &blob)
        .map_or_else(|| "?".to_owned(), |digest| split_hash(&digest))
}

/// `base64.b64decode(text)` with `validate=False`, which is what upstream
/// calls and is much more forgiving than it looks.
///
/// Anything outside the alphabet is discarded rather than refused -- newlines
/// and spaces included -- and only then is the padding checked. The two
/// `binascii` errors it can still raise are both `ValueError`, which
/// `_gen_fingerprint` turns into `"?"`, so this only has to say *whether* it
/// decoded.
fn py_b64decode(text: &str) -> Option<Vec<u8>> {
    let kept: String = text
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
        .collect();
    let split = kept.find('=').unwrap_or(kept.len());
    let data = kept.get(..split).unwrap_or_default();
    let pads = kept
        .get(split..)
        .unwrap_or_default()
        .chars()
        .take_while(|ch| *ch == '=')
        .count();

    let over = data.len() % 4;
    if over == 1 {
        // "number of data characters cannot be 1 more than a multiple of 4".
        return None;
    }
    // The missing characters have to be *written* as padding: a bare "AAA" is
    // "Incorrect padding" even though its length is unambiguous.
    if over != 0 && pads < 4 - over {
        return None;
    }
    ci_core::b64::decode(data)
}

/// `_is_printable_key`.
#[must_use]
pub fn is_printable_key(entry: &Entry) -> bool {
    let any = !entry.keytype.is_empty()
        || !entry.base64.is_empty()
        || !entry.comment.is_empty()
        || !entry.options.is_empty();
    any && ci_ssh::VALID_KEY_TYPES.contains(&entry.keytype.to_lowercase().trim())
}

/// `simpletable.SimpleTable`.
#[derive(Debug, Clone)]
struct SimpleTable {
    fields: Vec<String>,
    rows: Vec<Vec<String>>,
    widths: Vec<usize>,
}

impl SimpleTable {
    fn new(fields: &[&str]) -> Self {
        let fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
        let mut table = Self {
            widths: vec![0; fields.len()],
            rows: Vec::new(),
            fields: fields.clone(),
        };
        table.update_widths(&fields);
        table
    }

    /// `update_column_widths`, which measures in *characters* because that is
    /// what `len()` counts for a `str`.
    fn update_widths(&mut self, values: &[String]) {
        for (index, value) in values.iter().enumerate() {
            if let Some(width) = self.widths.get_mut(index) {
                *width = (*width).max(value.chars().count());
            }
        }
    }

    fn add_row(&mut self, values: Vec<String>) {
        self.update_widths(&values);
        self.rows.push(values);
    }

    fn hdiv(&self) -> String {
        let inner: Vec<String> =
            self.widths.iter().map(|w| "-".repeat(w + 2)).collect();
        format!("+{}+", inner.join("+"))
    }

    fn row(&self, row: &[String]) -> String {
        let inner: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, col)| {
                str_center(col, self.widths.get(index).copied().unwrap_or(0) + 2)
            })
            .collect();
        format!("|{}|", inner.join("|"))
    }

    fn get_string(&self) -> String {
        let mut lines = vec![self.hdiv(), self.row(&self.fields), self.hdiv()];
        lines.extend(self.rows.iter().map(|row| self.row(row)));
        lines.push(self.hdiv());
        lines.join("\n")
    }
}

/// `str.center(width)`, which puts the *extra* space on the left:
/// `"ab".center(5) == "  ab "`.
#[expect(clippy::integer_division, reason = "CPython's own halving")]
fn str_center(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_owned();
    }
    let margin = width - len;
    // CPython: `left = marg // 2 + (marg & width & 1)`.
    let left = margin / 2 + (margin & width & 1);
    format!("{}{text}{}", " ".repeat(left), " ".repeat(margin - left))
}

/// `util.center(text, fill, max_len)`, which is the *format spec* `^` and so
/// puts the extra fill on the right -- the opposite of [`str_center`].
#[must_use]
#[expect(clippy::integer_division, reason = "CPython's own halving")]
pub fn center(text: &str, fill: char, max_len: usize) -> String {
    let len = text.chars().count();
    if len >= max_len {
        return text.to_owned();
    }
    let margin = max_len - len;
    let left = margin / 2;
    format!(
        "{}{text}{}",
        fill.to_string().repeat(left),
        fill.to_string().repeat(margin - left)
    )
}

/// `_pprint_key_entries`.
pub fn pprint_key_entries(
    host: &mut dyn Host,
    user: &str,
    key_fn: &str,
    entries: &[Entry],
    hash_meth: &str,
) {
    if entries.is_empty() {
        host.multi_log(&format!(
            "{PREFIX}no authorized SSH keys fingerprints found for user \
             {user}.\n"
        ));
        return;
    }

    let mut table = SimpleTable::new(&[
        "Keytype",
        &format!("Fingerprint ({hash_meth})"),
        "Options",
        "Comment",
    ]);
    for entry in entries {
        if is_printable_key(entry) {
            table.add_row(vec![
                dash(&entry.keytype),
                dash(&gen_fingerprint(&entry.base64, hash_meth)),
                dash(&entry.options),
                dash(&entry.comment),
            ]);
        }
    }

    let table_text = table.get_string();
    let table_lines: Vec<&str> = table_text.split('\n').collect();
    // `len(max(lines, key=len))` -- the first longest line, measured in
    // characters.
    let max_len = table_lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = vec![center(
        &format!("Authorized keys from {key_fn} for user {user}"),
        '+',
        max_len,
    )];
    lines.extend(table_lines.iter().map(|line| (*line).to_owned()));
    for line in lines {
        host.multi_log(&format!("{PREFIX}{line}\n"));
    }
}

/// `x or "-"`, which is Python truthiness rather than emptiness -- but every
/// field here is a string, so the two coincide.
fn dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text.to_owned()
    }
}

/// `handle`, against a scripted machine.
pub fn handle_with(
    name: &str,
    cfg: &Object,
    users: &Object,
    host: &mut dyn Host,
    log: &mut Logger,
) {
    if ci_config::option::is_true(
        cfg.get("no_ssh_fingerprints")
            .unwrap_or(&Value::Bool(false)),
    ) {
        log.debug(
            SOURCE,
            &format!(
                "Skipping module named {name}, logging of SSH fingerprints \
                 disabled"
            ),
        );
        return;
    }

    let hash_meth = cfg
        .get("authkey_hash")
        .map_or_else(|| "sha256".to_owned(), super::py_str);

    for (user_name, user_cfg) in users {
        // `_cfg.get(...)` on both, so a user config that is not a mapping is
        // simply not skipped.
        let skip = user_cfg
            .get("no_create_home")
            .is_some_and(ci_config::option::py_truthy)
            || user_cfg
                .get("system")
                .is_some_and(ci_config::option::py_truthy);
        if skip {
            log.debug(
                SOURCE,
                &format!(
                    "Skipping printing of ssh fingerprints for user \
                     '{user_name}' because no home directory is created"
                ),
            );
            continue;
        }

        let (key_fn, entries) = host.authorized_keys(user_name);
        pprint_key_entries(host, user_name, &key_fn, &entries, &hash_meth);
    }
}

/// The registry entry point.
///
/// # Errors
/// Whatever `normalize_users_groups` raised; `handle` itself raises nothing.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let default_user = args.system_info.get("default_user").cloned();
    let normalized = ci_distro::ug::normalize_users_groups(
        args.cfg,
        default_user.as_ref(),
        args.logger,
    )?;
    let mut host = Live {
        root: args.root.to_owned(),
    };
    let (name, cfg) = (args.name.to_owned(), args.cfg.clone());
    handle_with(&name, &cfg, &normalized.users, &mut host, &mut *args.logger);
    Ok(())
}

/// The real machine.
#[derive(Debug, Clone)]
pub struct Live {
    root: std::path::PathBuf,
}

impl Host for Live {
    fn authorized_keys(&mut self, user: &str) -> (String, Vec<Entry>) {
        let mut quiet = Logger::silent();
        ci_ssh::install::extract_authorized_keys(
            &self.root,
            user,
            ci_ssh::DEF_SSHD_CFG,
            &mut quiet,
        )
        .map_or_else(
            |_| (String::new(), Vec::new()),
            |(path, lines)| {
                let entries = lines
                    .iter()
                    .map(|line| Entry {
                        keytype: line.keytype.clone(),
                        base64: line.base64.clone(),
                        comment: line.comment.clone(),
                        options: line.options.clone(),
                    })
                    .collect();
                (path, entries)
            },
        )
    }

    fn multi_log(&mut self, text: &str) {
        super::multi_log_console(text);
    }
}

/// A [`Host`] whose every answer is set up front, for the differential.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// User name to the file that was chosen and what was in it.
    pub keys: Vec<(String, (String, Vec<Entry>))>,
    pub calls: Vec<String>,
    /// What reached the console, one call per line.
    pub console: Vec<String>,
}

impl Host for Fixture {
    fn authorized_keys(&mut self, user: &str) -> (String, Vec<Entry>) {
        self.calls.push(format!("authorized_keys {user}"));
        self.keys
            .iter()
            .find(|(name, _)| name == user)
            .map_or_else(|| (String::new(), Vec::new()), |(_, found)| found.clone())
    }

    fn multi_log(&mut self, text: &str) {
        self.console.push(text.to_owned());
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn entry(keytype: &str, base64: &str, comment: &str) -> Entry {
        Entry {
            keytype: keytype.to_owned(),
            base64: base64.to_owned(),
            comment: comment.to_owned(),
            options: String::new(),
        }
    }

    #[test]
    fn the_two_centring_rules_differ_in_which_side_gets_the_extra_space() {
        // `str.center` pads left first, the format spec pads right first.
        assert_eq!(str_center("ab", 5), "  ab ");
        assert_eq!(center("ab", '+', 5), "+ab++");
        assert_eq!(str_center("ab", 6), "  ab  ");
        assert_eq!(center("ab", '+', 6), "++ab++");
        assert_eq!(str_center("abcd", 3), "abcd");
        assert_eq!(center("abcd", '+', 3), "abcd");
    }

    #[test]
    fn a_fingerprint_is_the_digest_in_byte_sized_pieces() {
        assert_eq!(
            gen_fingerprint("AAAA", "md5"),
            "69:3e:9a:f8:4d:3d:fc:c7:1e:64:0e:00:5b:dc:5e:2e"
        );
        assert_eq!(
            gen_fingerprint("AAAA", "sha256"),
            "70:9e:80:c8:84:87:a2:41:1e:1e:e4:df:b9:f2:2a:86:14:92:d2:0c:47:\
             65:15:0c:0c:79:4a:bd:70:f8:14:7c"
        );
        assert_eq!(gen_fingerprint("", "sha256"), "");
        assert_eq!(gen_fingerprint("!!!!not base64!!!!", "sha256"), "?");
        // Base64 that cannot be padded into whole bytes.
        assert_eq!(gen_fingerprint("A", "sha256"), "?");
        assert_eq!(gen_fingerprint("AAA", "sha256"), "?");
        // `hashlib` knows sha512 and this port does not, so it answers the
        // way it does for a name that does not exist -- deviation 163.
        assert_eq!(gen_fingerprint("AAAA", "sha512"), "?");
        assert_eq!(gen_fingerprint("AAAA", "nosuchhash"), "?");
    }

    #[test]
    fn a_key_of_an_unknown_type_is_not_printed() {
        assert!(is_printable_key(&entry("ssh-rsa", "AAAA", "root@h")));
        assert!(is_printable_key(&entry("SSH-RSA", "AAAA", "")));
        assert!(!is_printable_key(&entry("ssh-bogus", "AAAA", "")));
        assert!(!is_printable_key(&entry("", "", "")));
        // Every field empty but the type is still not enough on its own.
        assert!(!is_printable_key(&Entry::default()));
    }

    #[test]
    fn a_user_with_no_keys_gets_one_line_instead_of_a_table() {
        let mut host = Fixture::default();
        let mut log = Logger::capturing();
        let users = serde_json::json!({"ubuntu": {}});
        handle_with(
            "ssh_authkey_fingerprints",
            &Object::new(),
            users.as_object().unwrap(),
            &mut host,
            &mut log,
        );
        assert_eq!(
            host.console,
            [
                "ci-info: no authorized SSH keys fingerprints found for user \
              ubuntu.\n"
            ]
        );
    }

    #[test]
    fn the_table_is_boxed_and_the_title_is_padded_with_plus_signs() {
        let mut host = Fixture {
            keys: vec![(
                "ubuntu".to_owned(),
                (
                    "/home/ubuntu/.ssh/authorized_keys".to_owned(),
                    vec![entry("ssh-rsa", "AAAA", "root@h")],
                ),
            )],
            ..Fixture::default()
        };
        let mut log = Logger::capturing();
        let users = serde_json::json!({"ubuntu": {}});
        handle_with(
            "ssh_authkey_fingerprints",
            &Object::new(),
            users.as_object().unwrap(),
            &mut host,
            &mut log,
        );
        let printed: Vec<&str> = host.console.iter().map(String::as_str).collect();
        // Title, top rule, header, rule, one row, bottom rule.
        assert_eq!(printed.len(), 6);
        assert!(printed[0].starts_with("ci-info: +"));
        assert!(printed[0].contains(
            "Authorized keys from /home/ubuntu/.ssh/authorized_keys for user \
             ubuntu"
        ));
        assert!(printed[2].contains("Fingerprint (sha256)"));
        assert!(printed[4].contains("ssh-rsa"));
        // No options on the entry, so the column shows the placeholder.
        assert!(printed[4].contains(" - "));
    }

    #[test]
    fn a_system_user_is_skipped_before_its_keys_are_read() {
        let mut host = Fixture::default();
        let mut log = Logger::capturing();
        let users = serde_json::json!({
            "svc": {"system": true},
            "nohome": {"no_create_home": true},
        });
        handle_with(
            "ssh_authkey_fingerprints",
            &Object::new(),
            users.as_object().unwrap(),
            &mut host,
            &mut log,
        );
        assert!(host.calls.is_empty());
        assert_eq!(log.captured().len(), 2);
    }

    #[test]
    fn no_ssh_fingerprints_stops_the_module() {
        let mut host = Fixture::default();
        let mut log = Logger::capturing();
        let cfg = serde_json::json!({"no_ssh_fingerprints": true});
        let users = serde_json::json!({"ubuntu": {}});
        handle_with(
            "ssh_authkey_fingerprints",
            cfg.as_object().unwrap(),
            users.as_object().unwrap(),
            &mut host,
            &mut log,
        );
        assert!(host.calls.is_empty());
        assert!(host.console.is_empty());
    }
}
