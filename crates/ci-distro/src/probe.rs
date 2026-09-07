//! The distro facts `util.py` shells out for.
//!
//! Upstream keeps `lsb_release()` and `get_dpkg_architecture()` in `util`,
//! alongside everything else. They live here instead because both want to log,
//! and `ci-core` — where the rest of `util` landed — sits *below* `ci-log` in
//! the dependency order and so cannot. Nothing else about them changes.

use std::collections::BTreeMap;

use ci_log::Logger;

const SOURCE: &str = "util.py";

/// The four `lsb_release --all` fields upstream keeps, under its own names.
const LSB_FIELDS: [(&str, &str); 4] = [
    ("Codename", "codename"),
    ("Description", "description"),
    ("Distributor ID", "id"),
    ("Release", "release"),
];

/// `util.lsb_release()`.
///
/// A command that fails gives `UNAVAILABLE` for all four fields, so a caller
/// still gets a codename; a command that succeeds but omits a field leaves that
/// key absent, which is what makes upstream's `lsb_release()["codename"]` a
/// `KeyError` rather than a default.
///
/// Upstream memoises this with `lru_cache`. Nothing here does, because the only
/// caller asks once per boot.
#[must_use]
pub fn lsb_release(log: &mut Logger) -> BTreeMap<String, String> {
    let argv = vec!["lsb_release".to_owned(), "--all".to_owned()];
    let output = match ci_sys::subp::Subp::new(argv).inherit_env().check() {
        Ok(output) => output,
        Err(error) => {
            log.warning(SOURCE, &format!("Unable to get lsb_release --all: {error}"));
            return LSB_FIELDS
                .iter()
                .map(|(_, name)| ((*name).to_owned(), "UNAVAILABLE".to_owned()))
                .collect();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_lsb_release(&stdout, log)
}

fn parse_lsb_release(stdout: &str, log: &mut Logger) -> BTreeMap<String, String> {
    let mut data = BTreeMap::new();
    for line in ci_core::pystr::split_lines(stdout) {
        // `str.partition(":")` splits on the first colon only, and yields the
        // whole line as the head when there is none.
        let (fname, value) = line.split_once(':').unwrap_or((line, ""));
        if let Some((_, name)) = LSB_FIELDS.iter().find(|(key, _)| *key == fname) {
            data.insert((*name).to_owned(), value.trim().to_owned());
        }
    }
    let missing: Vec<&str> = LSB_FIELDS
        .iter()
        .map(|(_, name)| *name)
        .filter(|name| !data.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        log.warning(
            SOURCE,
            &format!(
                "Missing fields in lsb_release --all output: {}",
                missing.join(",")
            ),
        );
    }
    data
}

/// `util.get_dpkg_architecture()`.
///
/// # Errors
///
/// The `ProcessExecutionError` upstream lets escape when `dpkg` is missing or
/// fails.
pub fn get_dpkg_architecture() -> Result<String, String> {
    let argv = vec!["dpkg".to_owned(), "--print-architecture".to_owned()];
    let output = ci_sys::subp::Subp::new(argv)
        .inherit_env()
        .check()
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
    fn the_four_known_fields_are_renamed_and_the_rest_ignored() {
        let mut log = Logger::silent();
        let stdout = "No LSB modules are available.\n\
                      Distributor ID:\tUbuntu\n\
                      Description:\tUbuntu 26.04 LTS\n\
                      Release:\t26.04\n\
                      Codename:\tresolute\n";
        let data = parse_lsb_release(stdout, &mut log);
        assert_eq!(data["codename"], "resolute");
        assert_eq!(data["description"], "Ubuntu 26.04 LTS");
        assert_eq!(data["id"], "Ubuntu");
        assert_eq!(data["release"], "26.04");
        assert_eq!(data.len(), 4);
    }

    /// A description containing a colon must survive: `partition` splits once.
    #[test]
    fn only_the_first_colon_separates_the_field_from_its_value() {
        let mut log = Logger::silent();
        let data = parse_lsb_release("Description:\tUbuntu: the good one\n", &mut log);
        assert_eq!(data["description"], "Ubuntu: the good one");
    }

    #[test]
    fn a_field_the_command_did_not_print_is_simply_absent() {
        let mut log = Logger::silent();
        let data = parse_lsb_release("Codename:\tresolute\n", &mut log);
        assert_eq!(data.len(), 1);
        assert!(!data.contains_key("release"));
    }
}
