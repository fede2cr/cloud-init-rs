//! Port of `cloudinit/config/cc_write_files_deferred.py`.
//!
//! The other half of `write_files`. `cc_write_files` runs in the config stage
//! and writes every entry *without* `defer: true`; this one runs in the final
//! stage and writes exactly the entries the first pass skipped. The point of
//! the split is ordering: a file deferred to the final stage lands after
//! packages are installed and users exist, so it can name an owner that did
//! not exist earlier in the boot.
//!
//! It is undocumented in upstream's schema on purpose — `defer` on an entry is
//! the documented surface, and this module is the machinery behind it.

use super::write_files::{self, DEFAULT_DEFER};
use super::Args;

const SOURCE: &str = "cc_write_files_deferred.py";

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let files = write_files::entries(args.cfg)?;
    let deferred: Vec<_> = files
        .iter()
        .copied()
        .filter(|entry| write_files::option_bool(entry, "defer", DEFAULT_DEFER))
        .collect();
    if deferred.is_empty() {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!("Skipping module named {name}, no deferred file defined in configuration"),
        );
        return Ok(());
    }
    let owner = args.distro.default_owner.to_owned();
    let ssl = ci_url::fetch_ssl_details(args.paths);
    write_files::write_files(args, &deferred, &owner, &ssl)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::path::Path;

    use ci_config::Value;
    use ci_log::Logger;
    use serde_json::json;

    use super::*;

    fn run(root: &Path, cfg: &serde_json::Value) -> Result<(), String> {
        let cfg = cfg.as_object().unwrap();
        let paths = ci_core::Paths::default();
        let mut logger = Logger::silent();
        let empty = Value::Array(Vec::new());
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "write_files_deferred",
            cfg,
            args: &empty,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(crate::cc::tests::fixture_datasource()),
            logger: &mut logger,
        };
        handle(&mut args)
    }

    #[test]
    fn only_the_deferred_entries_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let now = dir.path().join("now");
        let later = dir.path().join("later");
        run(
            dir.path(),
            &json!({"write_files": [
                {"path": now, "content": "a", "owner": "-1:-1"},
                {"path": later, "content": "b", "owner": "-1:-1", "defer": true},
            ]}),
        )
        .unwrap();
        assert!(!now.exists());
        assert_eq!(std::fs::read_to_string(&later).unwrap(), "b");
    }

    #[test]
    fn nothing_deferred_is_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), &json!({})), Ok(()));
        assert_eq!(
            run(
                dir.path(),
                &json!({"write_files": [{"path": dir.path().join("x"), "content": "a"}]})
            ),
            Ok(())
        );
        assert!(!dir.path().join("x").exists());
    }
}
