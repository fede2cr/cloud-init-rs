//! Pathname expansion.
//!
//! `check_config` and two of the datasource checks turn globbing back on for a
//! moment (`set +f; set -- $files; set -f`), so the port needs the same
//! expansion: component-wise matching, sorted results, and a word that matches
//! nothing left standing as a literal.

use std::path::{Path, PathBuf};

use crate::shell::glob_match;

/// True when the word contains a character that makes the shell expand it.
#[must_use]
pub fn is_pattern(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

/// Expands one word. A word matching nothing comes back unchanged.
#[must_use]
pub fn expand(word: &str) -> Vec<String> {
    if !is_pattern(word) {
        return vec![word.to_owned()];
    }
    let absolute = word.starts_with('/');
    let mut current: Vec<PathBuf> =
        vec![PathBuf::from(if absolute { "/" } else { "." })];
    let mut matched_anything = true;
    for component in word.split('/').filter(|c| !c.is_empty()) {
        if is_pattern(component) {
            let mut next = Vec::new();
            for dir in &current {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut names: Vec<String> = entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    // A leading dot is only matched by an explicit leading dot.
                    .filter(|name| !name.starts_with('.') || component.starts_with('.'))
                    .filter(|name| glob_match(component, name))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|name| dir.join(name)));
            }
            current = next;
        } else {
            current = current
                .into_iter()
                .map(|dir| dir.join(component))
                .filter(|p| exists(p))
                .collect();
        }
        if current.is_empty() {
            matched_anything = false;
            break;
        }
    }
    if !matched_anything {
        return vec![word.to_owned()];
    }
    let mut out: Vec<String> = current
        .into_iter()
        .map(|p| {
            let text = p.to_string_lossy().into_owned();
            if absolute {
                text
            } else {
                text.strip_prefix("./").unwrap_or(&text).to_owned()
            }
        })
        .collect();
    out.sort();
    out
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
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
    fn a_word_that_matches_nothing_stays_literal() {
        let word = "/nonexistent-ds-identify-test/*.cfg";
        assert_eq!(expand(word), vec![word.to_owned()]);
    }

    #[test]
    fn a_plain_word_is_not_touched() {
        assert_eq!(expand("/etc/cloud/cloud.cfg"), vec!["/etc/cloud/cloud.cfg"]);
    }

    #[test]
    fn expands_and_sorts() {
        let dir = ci_sys::path::TempDir::new(std::env::temp_dir(), "ds-identify-glob")
            .unwrap();
        for name in ["b.cfg", "a.cfg", "c.txt"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let pattern = format!("{}/*.cfg", dir.path().display());
        let got = expand(&pattern);
        assert_eq!(got.len(), 2);
        assert!(got[0].ends_with("a.cfg"));
        assert!(got[1].ends_with("b.cfg"));
    }
}
