//! `Distro.update_etc_hosts` and the `/etc/hosts` parser it round-trips
//! through.
//!
//! Like [`crate::hostname`], the point of the parser is that the file belongs
//! to the machine rather than to cloud-init. `/etc/hosts` is edited by hand,
//! by packages and by other tools, so the update reads it, changes the one
//! entry it is responsible for, and writes everything else back untouched —
//! comments, blank lines and the tabs-or-spaces of the original all included.

use std::path::Path;

use ci_sys::atomic::{self, WriteOptions};

use crate::{Distro, HOSTS_FN};

/// `distros/parsers/hosts.py`: `/etc/hosts` with its comments intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsConf {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A blank line, kept verbatim.
    Blank(String),
    /// A line that is nothing but a comment, kept verbatim.
    Comment(String),
    /// An address and its names, plus whatever trailing comment followed.
    Option { pieces: Vec<String>, tail: String },
}

impl HostsConf {
    /// `HostsConf(text).parse()`.
    ///
    /// Unlike its hostname counterpart this cannot fail: every line either
    /// parses or is kept verbatim.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut entries = Vec::new();
        for line in ci_core::pystr::split_lines(text) {
            if ci_core::pystr::strip(line).is_empty() {
                entries.push(Entry::Blank(line.to_owned()));
                continue;
            }
            let (head, tail) = chop_comment(ci_core::pystr::strip(line));
            if head.is_empty() {
                entries.push(Entry::Comment(line.to_owned()));
                continue;
            }
            entries.push(Entry::Option {
                pieces: ci_core::pystr::split_whitespace_n(head, usize::MAX)
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
                tail: tail.to_owned(),
            });
        }
        Self { entries }
    }

    /// `conf.get_entry(ip)`: the names each line for `ip` carries.
    #[must_use]
    pub fn get_entry(&self, ip: &str) -> Vec<Vec<String>> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Option { pieces, .. }
                    if pieces.first().map(String::as_str) == Some(ip) =>
                {
                    Some(pieces.get(1..).unwrap_or_default().to_vec())
                }
                _ => None,
            })
            .collect()
    }

    /// `conf.del_entries(ip)`.
    pub fn del_entries(&mut self, ip: &str) {
        self.entries.retain(|entry| match entry {
            Entry::Option { pieces, .. } => {
                pieces.first().map(String::as_str) != Some(ip)
            }
            _ => true,
        });
    }

    /// `conf.add_entry(ip, canonical, *aliases)`, appended at the end.
    pub fn add_entry(&mut self, ip: &str, names: &[String]) {
        let mut pieces = vec![ip.to_owned()];
        pieces.extend_from_slice(names);
        self.entries.push(Entry::Option {
            pieces,
            tail: String::new(),
        });
    }
}

impl std::fmt::Display for HostsConf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for entry in &self.entries {
            match entry {
                Entry::Blank(line) | Entry::Comment(line) => writeln!(f, "{line}")?,
                Entry::Option { pieces, tail } => {
                    writeln!(f, "{}{tail}", pieces.join("\t"))?;
                }
            }
        }
        Ok(())
    }
}

fn chop_comment(text: &str) -> (&str, &str) {
    match text.find('#') {
        Some(at) => text.split_at(at),
        None => (text, ""),
    }
}

/// `Distro.update_etc_hosts`: make sure the loopback address names this host.
///
/// Only the distro's own loopback address is touched — `127.0.1.1` on Debian
/// and friends, `127.0.0.1` elsewhere — and only if it does not already carry
/// the pair. Everything else in the file survives.
///
/// # Errors
/// A `/etc/hosts` that cannot be read or written. A read failure is fatal
/// rather than treated as "no file": upstream reaches this through
/// `os.path.exists`, so an existing-but-unreadable file raises there too, and
/// silently starting from an empty file would drop every entry the machine
/// has.
pub fn update_etc_hosts(
    distro: &Distro,
    root: &Path,
    hostname: &str,
    fqdn: &str,
) -> Result<(), String> {
    let path = root.join(HOSTS_FN.trim_start_matches('/'));
    let (mut conf, header) = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        (HostsConf::parse(&text), None)
    } else {
        (
            HostsConf::parse(""),
            Some(ci_core::version::make_header('#', "added")),
        )
    };

    let local_ip = distro.localhost_ip;
    let previous = conf.get_entry(local_ip);
    let mut need_change;
    if previous.is_empty() {
        conf.add_entry(local_ip, &[fqdn.to_owned(), hostname.to_owned()]);
        need_change = true;
    } else {
        need_change = true;
        for entry in &previous {
            // An entry of one name has no aliases, so it can never satisfy
            // this even when that one name is the FQDN.
            if entry.first().map(String::as_str) == Some(fqdn)
                && entry
                    .get(1..)
                    .unwrap_or_default()
                    .iter()
                    .any(|a| a == hostname)
            {
                need_change = false;
            }
        }
        if need_change {
            let mut rewritten = previous.clone();
            rewritten.push(vec![fqdn.to_owned(), hostname.to_owned()]);
            conf.del_entries(local_ip);
            for entry in &rewritten {
                // A bare `127.0.0.1` with no names at all is dropped here,
                // upstream included: neither arm of its `if` matches.
                if !entry.is_empty() {
                    conf.add_entry(local_ip, entry);
                }
            }
        }
    }

    if !need_change {
        return Ok(());
    }

    let mut contents = String::new();
    if let Some(header) = header {
        contents.push_str(&header);
        contents.push('\n');
    }
    // `contents.write("%s\n" % eh)`: `HostsConf` already ends every line with
    // a newline, so the file upstream writes ends with a blank line.
    contents.push_str(&conf.to_string());
    contents.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    atomic::write_file(
        &path,
        contents.as_bytes(),
        WriteOptions {
            mode: 0o644,
            ..WriteOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", path.display()))
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

    fn ubuntu() -> &'static Distro {
        crate::fetch("ubuntu").unwrap()
    }

    fn written(root: &Path) -> String {
        std::fs::read_to_string(root.join("etc/hosts")).unwrap()
    }

    fn fixture(contents: Option<&str>) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        if let Some(contents) = contents {
            std::fs::write(root.path().join("etc/hosts"), contents).unwrap();
        }
        root
    }

    #[test]
    fn a_line_survives_its_own_spacing_and_trailing_comment() {
        let text = "# a comment\n\n  127.0.0.1   localhost  # why\n";
        let conf = HostsConf::parse(text);
        // The pieces are re-joined with tabs and the whitespace before the
        // comment belongs to the head, which is split away — so `localhost`
        // and `# why` end up adjacent. Verified against upstream.
        assert_eq!(
            conf.to_string(),
            "# a comment\n\n127.0.0.1\tlocalhost# why\n"
        );
    }

    #[test]
    fn get_entry_returns_the_names_of_every_line_for_one_address() {
        let conf = HostsConf::parse("127.0.1.1 a b\n192.0.2.1 c\n127.0.1.1 d\n");
        assert_eq!(
            conf.get_entry("127.0.1.1"),
            vec![vec!["a".to_owned(), "b".to_owned()], vec!["d".to_owned()]]
        );
        assert!(conf.get_entry("10.0.0.1").is_empty());
    }

    #[test]
    fn an_entry_that_already_names_the_pair_is_left_alone() {
        let root = fixture(Some("127.0.1.1\th1.example.com\th1\n192.0.2.5\tother\n"));
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        assert_eq!(
            written(root.path()),
            "127.0.1.1\th1.example.com\th1\n192.0.2.5\tother\n"
        );
    }

    #[test]
    fn the_canonical_name_alone_is_not_enough_the_alias_has_to_be_there_too() {
        let root = fixture(Some("127.0.1.1\th1.example.com\n"));
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        // Rewritten in place: the old entry is re-added first, then the pair.
        assert_eq!(
            written(root.path()),
            "127.0.1.1\th1.example.com\n127.0.1.1\th1.example.com\th1\n\n"
        );
    }

    #[test]
    fn a_loopback_line_with_no_names_at_all_is_dropped_on_rewrite() {
        let root = fixture(Some("127.0.1.1\n192.0.2.5\tother\n"));
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        assert_eq!(
            written(root.path()),
            "192.0.2.5\tother\n127.0.1.1\th1.example.com\th1\n\n"
        );
    }

    #[test]
    fn only_the_distros_own_loopback_address_is_touched() {
        // ubuntu manages 127.0.1.1, so the 127.0.0.1 line is untouched and a
        // new entry is appended rather than merged into it.
        let root = fixture(Some("127.0.0.1\tlocalhost\n"));
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        assert_eq!(
            written(root.path()),
            "127.0.0.1\tlocalhost\n127.0.1.1\th1.example.com\th1\n\n"
        );
    }

    #[test]
    fn a_missing_file_gains_a_header_and_the_one_entry() {
        let root = fixture(None);
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        let text = written(root.path());
        assert!(text.starts_with("# Added by cloud-init v. "), "{text}");
        assert!(
            text.ends_with("\n127.0.1.1\th1.example.com\th1\n\n"),
            "{text}"
        );
    }

    #[test]
    fn comments_and_blank_lines_survive_a_rewrite() {
        let root = fixture(Some("# keep\n\n127.0.1.1\told.example.com\told\n"));
        update_etc_hosts(ubuntu(), root.path(), "h1", "h1.example.com").unwrap();
        assert_eq!(
            written(root.path()),
            "# keep\n\n127.0.1.1\told.example.com\told\n127.0.1.1\th1.example.com\th1\n\n"
        );
    }
}
