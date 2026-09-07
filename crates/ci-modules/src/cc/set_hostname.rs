//! Port of `cloudinit/config/cc_set_hostname.py`.
//!
//! Names the machine. Small, and one of the few modules whose failure is
//! allowed to fail the boot — most `cc_*` bodies log and step over, this one
//! raises, because a machine that came up under the wrong name is worse than
//! one that stopped and said so.
//!
//! It is also careful not to do the work twice. The pair it last set is kept
//! in `<cloud_dir>/data/set-hostname`, and a boot that resolves the same pair
//! returns before touching anything — which is what lets a tenant who
//! renamed the machine by hand keep the new name across a reboot.

use std::path::Path;

use ci_config::{option, Object, Value};
use ci_core::hostname::HostnameFqdn;
use ci_core::paths::Lookup;
use ci_sys::atomic::{self, WriteOptions};

use super::Args;

const SOURCE: &str = "cc_set_hostname.py";

/// `helpers.DEF_PERMS`, the mode the previous-hostname record is written with.
const DEF_PERMS: u32 = 0o644;

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    if option::get_bool(args.cfg, "preserve_hostname", false) {
        let message = format!(
            "Configuration option 'preserve_hostname' is set, not setting the \
             hostname in module {}",
            args.name
        );
        args.debug(SOURCE, &message);
        return Ok(());
    }

    // `prefer_fqdn_over_hostname` and `create_hostname_file` are copied onto
    // `distro._cfg` upstream so that the distro methods can read them back.
    // The distro table here is static, so both keys are read straight out of
    // the config at the point of use instead — `select_hostname` and
    // `write_hostname` already do exactly that. The only observable
    // difference would be a later module reading the option off the distro;
    // none do.
    let metadata = args.datasource.map(|ds| ds.metadata);
    let resolved = ci_core::hostname::get_hostname_fqdn(args.cfg, metadata, args.root);

    let previous_path = args.paths.cpath(Lookup::Data).join("set-hostname");
    let previous = read_previous(&previous_path);
    if !changed(&resolved, previous.as_ref()) {
        args.debug(SOURCE, "No hostname changes. Skipping set_hostname");
        return Ok(());
    }

    // `localhost` that nobody asked for is left alone: systemd will pick a
    // transient name, and overwriting it here would stick.
    if resolved.is_default && resolved.hostname == "localhost" {
        args.debug(
            SOURCE,
            "Hostname is localhost. Let other services handle this.",
        );
        return Ok(());
    }

    let message = format!(
        "Setting the hostname to {} ({})",
        resolved.fqdn, resolved.hostname
    );
    args.debug(SOURCE, &message);

    if let Err(error) = ci_distro::hostname::set_hostname(
        args.distro,
        args.cfg,
        args.root,
        Some(&resolved.hostname),
        Some(&resolved.fqdn),
        args.logger,
    ) {
        return Err(format!(
            "Failed to set the hostname to {} ({}): {error}",
            resolved.fqdn, resolved.hostname
        ));
    }

    write_previous(&previous_path, &resolved)
}

/// The `{"hostname": ..., "fqdn": ...}` of the last successful run.
///
/// Anything unreadable, empty or not an object is `{}` upstream, which makes
/// the comparison below fail and the hostname get set again — the safe way
/// round for a record that only exists to skip work.
fn read_previous(path: &Path) -> Option<Object> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.is_empty() {
        return None;
    }
    match ci_core::jsonfmt::json_loads(&text) {
        Some(Value::Object(object)) => Some(object),
        _ => None,
    }
}

fn changed(resolved: &HostnameFqdn, previous: Option<&Object>) -> bool {
    let field = |key: &str| {
        previous
            .and_then(|object| object.get(key))
            .and_then(Value::as_str)
    };
    field("hostname") != Some(resolved.hostname.as_str())
        || field("fqdn") != Some(resolved.fqdn.as_str())
}

fn write_previous(path: &Path, resolved: &HostnameFqdn) -> Result<(), String> {
    let mut record = Object::new();
    record.insert(
        "hostname".to_owned(),
        Value::from(resolved.hostname.clone()),
    );
    record.insert("fqdn".to_owned(), Value::from(resolved.fqdn.clone()));
    let text = format!("{}\n", ci_core::jsonfmt::json_dumps(&Value::Object(record)));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    atomic::write_file(
        path,
        text.as_bytes(),
        WriteOptions {
            mode: DEF_PERMS,
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
    use crate::cc::tests::{fixture_datasource, fixture_distro};

    struct Fixture {
        dir: tempfile::TempDir,
        paths: ci_core::Paths,
    }

    impl Fixture {
        fn new(system_hostname: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("proc/sys/kernel")).unwrap();
            std::fs::create_dir_all(dir.path().join("etc")).unwrap();
            std::fs::write(
                dir.path().join("proc/sys/kernel/hostname"),
                system_hostname,
            )
            .unwrap();
            let cfg: Object = serde_json::json!({
                "system_info": {"paths": {"cloud_dir": dir.path().join("cloud")}}
            })
            .as_object()
            .unwrap()
            .clone();
            let paths = ci_core::Paths::from_config(&cfg);
            Self { dir, paths }
        }

        fn run(&self, cfg: &Object, log: &mut ci_log::Logger) -> Result<(), String> {
            let ds = fixture_datasource();
            let mut args = Args {
                system_info: crate::cc::tests::no_system_info(),
                name: "set_hostname",
                cfg,
                args: &Value::Null,
                paths: &self.paths,
                root: self.dir.path(),
                distro: fixture_distro(),
                datasource: Some(ds),
                logger: log,
            };
            handle(&mut args)
        }

        fn written(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join("etc/hostname")).ok()
        }

        fn record(&self) -> Option<String> {
            std::fs::read_to_string(self.paths.cpath(Lookup::Data).join("set-hostname"))
                .ok()
        }
    }

    fn cfg(pairs: &[(&str, Value)]) -> Object {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn a_configured_fqdn_is_written_and_recorded() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(&cfg(&[("fqdn", "host1.example.com".into())]), &mut log)
            .unwrap();
        assert_eq!(fixture.written().as_deref(), Some("host1\n"));
        assert_eq!(
            fixture.record().as_deref(),
            Some("{\n \"fqdn\": \"host1.example.com\",\n \"hostname\": \"host1\"\n}\n")
        );
    }

    #[test]
    fn preserve_hostname_writes_nothing() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(
                &cfg(&[
                    ("fqdn", "host1.example.com".into()),
                    ("preserve_hostname", true.into()),
                ]),
                &mut log,
            )
            .unwrap();
        assert_eq!(fixture.written(), None);
        assert_eq!(fixture.record(), None);
    }

    #[test]
    fn a_second_run_with_the_same_names_does_not_rewrite_the_file() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        let cfg = cfg(&[("fqdn", "host1.example.com".into())]);
        fixture.run(&cfg, &mut log).unwrap();
        // A tenant renaming the machine by hand must survive the next boot.
        std::fs::write(fixture.dir.path().join("etc/hostname"), "renamed\n").unwrap();
        fixture.run(&cfg, &mut log).unwrap();
        assert_eq!(fixture.written().as_deref(), Some("renamed\n"));
    }

    #[test]
    fn a_changed_fqdn_does_rewrite_it() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(&cfg(&[("fqdn", "host1.example.com".into())]), &mut log)
            .unwrap();
        fixture
            .run(&cfg(&[("fqdn", "host2.example.com".into())]), &mut log)
            .unwrap();
        assert_eq!(fixture.written().as_deref(), Some("host2\n"));
    }

    #[test]
    fn an_unasked_for_localhost_is_left_to_systemd() {
        let fixture = Fixture::new("localhost");
        let mut log = ci_log::Logger::silent();
        fixture.run(&Object::new(), &mut log).unwrap();
        assert_eq!(fixture.written(), None);
        assert_eq!(
            fixture.record(),
            None,
            "nothing was set, so nothing is recorded as set"
        );
    }

    #[test]
    fn an_explicitly_requested_localhost_is_honoured() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(&cfg(&[("hostname", "localhost".into())]), &mut log)
            .unwrap();
        assert_eq!(fixture.written().as_deref(), Some("localhost\n"));
    }

    #[test]
    fn prefer_fqdn_over_hostname_writes_the_long_name() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(
                &cfg(&[
                    ("fqdn", "host1.example.com".into()),
                    ("prefer_fqdn_over_hostname", true.into()),
                ]),
                &mut log,
            )
            .unwrap();
        assert_eq!(fixture.written().as_deref(), Some("host1.example.com\n"));
    }

    #[test]
    fn create_hostname_file_false_leaves_the_file_alone_but_still_records() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        fixture
            .run(
                &cfg(&[
                    ("fqdn", "host1.example.com".into()),
                    ("create_hostname_file", false.into()),
                ]),
                &mut log,
            )
            .unwrap();
        assert_eq!(fixture.written(), None);
        assert!(fixture.record().is_some());
    }

    #[test]
    fn a_corrupt_previous_record_is_treated_as_absent() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        let record = fixture.paths.cpath(Lookup::Data).join("set-hostname");
        std::fs::create_dir_all(record.parent().unwrap()).unwrap();
        std::fs::write(&record, "not json").unwrap();
        fixture
            .run(&cfg(&[("fqdn", "host1.example.com".into())]), &mut log)
            .unwrap();
        assert_eq!(fixture.written().as_deref(), Some("host1\n"));
    }

    #[test]
    fn metadata_names_the_machine_when_the_config_does_not() {
        let fixture = Fixture::new("old");
        let mut log = ci_log::Logger::silent();
        let metadata = cfg(&[("local-hostname", "from-metadata".into())]);
        let mut ds = fixture_datasource();
        ds.metadata = &metadata;
        let empty = Object::new();
        let mut args = Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "set_hostname",
            cfg: &empty,
            args: &Value::Null,
            paths: &fixture.paths,
            root: fixture.dir.path(),
            distro: fixture_distro(),
            datasource: Some(ds),
            logger: &mut log,
        };
        handle(&mut args).unwrap();
        assert_eq!(fixture.written().as_deref(), Some("from-metadata\n"));
    }
}
