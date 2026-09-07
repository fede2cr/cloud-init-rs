//! The `cc_*` modules themselves.
//!
//! Upstream a module is a Python file with a module-level `meta` dict and a
//! `handle(name, cfg, cloud, args)` function; [`registry`](crate::registry)
//! already holds the `meta` half for every one of them, and this is where the
//! `handle` half lands as each is ported.
//!
//! The split matters for the ones that are *not* ported yet. A name that
//! resolves in the registry is still selected, still ordered, still
//! semaphored and still reported — it simply has no body to run. So the
//! engine's behaviour does not change as this table fills in; only the effect
//! on the system does.

use std::path::{Path, PathBuf};

use ci_config::{type_name, Object, Value};
use ci_core::paths::Lookup;
use ci_core::Paths;
use ci_log::Logger;

pub mod ansible;
pub mod apt_configure;
pub mod apt_pipelining;
pub mod bootcmd;
pub mod ca_certs;
pub mod chef;
pub mod disk_setup;
pub mod final_message;
pub mod growpart;
pub mod keys_to_console;
pub mod locale;
pub mod mcollective;
pub mod mounts;
pub mod package_update_upgrade_install;
pub mod puppet;
pub mod resizefs;
pub mod rsyslog;
pub mod runcmd;
pub mod salt_minion;
pub mod scripts;
pub mod seed_random;
pub mod set_hostname;
pub mod set_passwords;
pub mod snap;
pub mod ssh;
pub mod ssh_authkey_fingerprints;
pub mod ssh_import_id;
pub mod timezone;
pub mod update_etc_hosts;
pub mod update_hostname;
pub mod users_groups;
pub mod write_files;
pub mod write_files_deferred;

/// The parts of `cloud.datasource` a ported module reads.
///
/// Grouped rather than spread across [`Args`] because they are absent
/// together: a stage that found no datasource has none of them, and a module
/// that reads one usually reads two.
#[derive(Debug, Clone, Copy)]
pub struct Datasource<'a> {
    /// `str(cloud.datasource)`, which upstream's `type_utils.obj_name` makes
    /// the Python *class* name — `DataSourceNoCloudNet`, not `NoCloud`.
    pub class_name: &'a str,
    /// `cloud.datasource.dsname`.
    pub dsname: &'a str,
    /// `cloud.get_instance_id()`.
    pub instance_id: &'a str,
    /// `cloud.datasource.metadata`.
    pub metadata: &'a Object,
    /// `cloud.datasource.sys_cfg`: the system config alone, which is not the
    /// same object as [`Args::cfg`] — user data has not been merged into it.
    pub sys_cfg: &'a Object,
    /// `cloud.get_public_ssh_keys()`, resolved once by the stage rather than
    /// per module: the accessor is a per-cloud override, so working it out
    /// needs the concrete datasource that the stage still has and `Args` does
    /// not.
    pub public_keys: &'a [String],
}

/// Everything upstream's `handle(name, cfg, cloud, args)` receives, minus the
/// `Cloud` object.
///
/// `Cloud` is a facade over the datasource, the distro and the paths, and
/// resolving it eagerly would mean every module paid for a datasource it
/// mostly does not use. The fields below are the parts of it that the ported
/// modules actually read; the rest arrives when a module needs it.
#[derive(Debug)]
pub struct Args<'a> {
    /// Upstream's `name`: the module's *configured* spelling, which is what
    /// its log lines quote. `- [write_files]` and `- [cc_write_files]` reach
    /// the same handler under two different names, and say so.
    pub name: &'a str,
    /// The merged config, minus `system_info` — `Modules.cfg` comes from
    /// `Init._extract_cfg("restricted")`, which pops that key before any
    /// module sees it. What lives under it reaches a module through
    /// [`Args::system_info`] instead.
    pub cfg: &'a Object,
    /// `cloud.distro._cfg`: the `system_info` block of the same merged
    /// config, which upstream hands to the distro object rather than to the
    /// module. `ci-distro`'s `Distro` is a static table row with nowhere to
    /// put per-boot config, so the block travels beside it.
    ///
    /// Note that this *is* the merged config's copy: `Init._read_cfg` folds
    /// `instance/cloud-config.txt` in, so user data can reach these keys
    /// upstream too. Treat anything read from here as tenant-supplied.
    pub system_info: &'a Object,
    /// The extra arguments the module section carried, if any. Upstream types
    /// this as `list` and no ported module has needed one yet.
    pub args: &'a Value,
    /// `cloud.paths`.
    pub paths: &'a Paths,
    /// The filesystem root that user and group names resolve against. `/` in
    /// production; a fixture directory in tests, so that a test can assert
    /// what ownership *would* have been applied without needing to be root.
    pub root: &'a Path,
    /// `cloud.distro`. The data half of it — see `ci-distro` for which half
    /// that is and why the rest arrives module by module.
    pub distro: &'a ci_distro::Distro,
    /// `cloud.datasource`, absent when no datasource is active.
    pub datasource: Option<Datasource<'a>>,
    pub logger: &'a mut Logger,
}

impl Args<'_> {
    /// Log at `WARNING` against the module's own source file, the way
    /// `logging.getLogger(__name__)` does inside each `cc_*`.
    fn warning(&mut self, source: &str, message: &str) {
        self.logger.warning(source, message);
    }

    fn debug(&mut self, source: &str, message: &str) {
        self.logger.debug(source, message);
    }

    fn info(&mut self, source: &str, message: &str) {
        self.logger.info(source, message);
    }

    /// `cloud.get_ipath(name)`, warning and returning `None` exactly as
    /// upstream does when there is no instance to hang the path off.
    fn ipath(&mut self, name: Lookup) -> Option<PathBuf> {
        if let Some(ds) = self.datasource {
            return Some(self.paths.instance_path_for(ds.instance_id, name));
        }
        self.warning(
            "helpers.py",
            "No per instance data available, is there an datasource/iid set?",
        );
        None
    }
}

/// `util.shellify`: a config's command list as the text of a `/bin/sh` script.
///
/// A string item is copied through verbatim, so it is shell *source* and may
/// contain redirections and pipes; a list item is one command whose arguments
/// are single-quoted individually, so nothing in it can be re-interpreted.
/// Those are two very different trust levels sharing one config key, which is
/// worth knowing before writing either into a root-run script.
///
/// Returns the script and the number of commands in it, which is what
/// upstream's `Shellified %s commands.` debug line counts.
fn shellify(commands: &Value) -> Result<(String, usize), String> {
    let Some(items) = commands.as_array() else {
        return Err(format!(
            "Input to shellify was type '{}'. Expected list or tuple.",
            type_name(commands)
        ));
    };
    let mut content = String::from("#!/bin/sh\n");
    let mut made = 0;
    for item in items {
        match item {
            // A YAML comment parses as null, and upstream drops it silently.
            Value::Null => {}
            Value::String(source) => {
                content.push_str(source);
                content.push('\n');
                made += 1;
            }
            Value::Array(argv) => {
                let quoted: Vec<String> = argv
                    .iter()
                    .map(|arg| format!("'{}'", py_str(arg).replace('\'', r"'\''")))
                    .collect();
                content.push_str(&quoted.join(" "));
                content.push('\n');
                made += 1;
            }
            other => {
                let (kind, shown) = (type_name(other), py_str(other));
                return Err(format!(
                    "Unable to shellify type '{kind}'. Expected list, string, tuple. Got: {shown}"
                ));
            }
        }
    }
    Ok((content, made))
}

/// Python's `str()` of a config scalar: a string is itself, anything else is
/// its repr.
pub(crate) fn py_str(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| ci_config::repr(value), ToOwned::to_owned)
}

/// `key in yobj` followed by `yobj[key]`, for a `yobj` that need not be a
/// mapping.
///
/// This is the opening of every `util.get_cfg_option_*`, and the reason a
/// module whose top-level key holds a string or a list gets one of Python's
/// own subscript errors rather than a message anyone wrote. The string arm is
/// a *substring* test, so whether it raises depends on the key being looked
/// up.
pub(super) fn sub_option(block: &Value, key: &str) -> Result<Option<Value>, String> {
    match block {
        Value::Object(map) => Ok(map.get(key).cloned()),
        Value::Array(items) => {
            if items.iter().any(|item| item.as_str() == Some(key)) {
                Err("list indices must be integers or slices, not str".to_owned())
            } else {
                Ok(None)
            }
        }
        Value::String(text) => {
            if text.contains(key) {
                Err("string indices must be integers, not 'str'".to_owned())
            } else {
                Ok(None)
            }
        }
        other => Err(format!(
            "argument of type '{}' is not a container or iterable",
            type_name(other)
        )),
    }
}

/// `yobj.get(key)`, which only a mapping has.
pub(super) fn dict_get(block: &Value, key: &str) -> Result<Option<Value>, String> {
    match block {
        Value::Object(map) => Ok(map.get(key).cloned()),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            type_name(other)
        )),
    }
}

/// `log_util.multi_log(text, stderr=False, console=True,
/// fallback_to_stdout=False)`, whose only remaining sink is the console.
fn multi_log_console(text: &str) {
    use std::io::Write as _;

    let console = Path::new("/dev/console");
    if console.exists() {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open(console)
            .and_then(|mut file| file.write_all(text.as_bytes()));
    }
}

/// `util.write_file(path, contents)` with everything left at its default.
///
/// The two defaults that matter are `mode=0o644`, applied whether or not the
/// file was already there because `preserve_mode` is false, and
/// `ensure_dir_exists=True`, which creates the parent rather than failing.
fn write_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    ci_sys::atomic::write_file(
        path,
        contents,
        ci_sys::atomic::WriteOptions {
            mode: 0o644,
            ..ci_sys::atomic::WriteOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}

/// Logical paths stay unrooted so that upstream's string comparisons and the
/// dumped plans match; the root is applied only where a syscall happens.
fn rooted(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// `distros.uses_systemd`: `/run/systemd/system` is a directory, not followed.
fn uses_systemd(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join("run/systemd/system"))
        .is_ok_and(|meta| meta.is_dir())
}

/// A module body. Failure is a string because that is what the semaphore
/// runner and the reporter carry, and what upstream puts in `status.json`.
pub type Handler = fn(&mut Args<'_>) -> Result<(), String>;

/// The ported modules, by import name, sorted.
///
/// Deliberately a separate table from [`MODULES`](crate::MODULES): that one is
/// generated from the installed cloud-init and is regenerated wholesale when
/// the reference version moves, so it must not grow hand-written fields.
const HANDLERS: &[(&str, Handler)] = &[
    ("cc_ansible", ansible::handle),
    ("cc_apt_configure", apt_configure::handle),
    ("cc_apt_pipelining", apt_pipelining::handle),
    ("cc_bootcmd", bootcmd::handle),
    ("cc_ca_certs", ca_certs::handle),
    ("cc_chef", chef::handle),
    ("cc_disk_setup", disk_setup::handle),
    ("cc_final_message", final_message::handle),
    ("cc_growpart", growpart::handle),
    ("cc_keys_to_console", keys_to_console::handle),
    ("cc_locale", locale::handle),
    ("cc_mcollective", mcollective::handle),
    ("cc_mounts", mounts::handle),
    (
        "cc_package_update_upgrade_install",
        package_update_upgrade_install::handle,
    ),
    ("cc_puppet", puppet::handle),
    ("cc_resizefs", resizefs::handle),
    ("cc_rsyslog", rsyslog::handle),
    ("cc_runcmd", runcmd::handle),
    ("cc_salt_minion", salt_minion::handle),
    ("cc_scripts_per_boot", scripts::per_boot),
    ("cc_scripts_per_instance", scripts::per_instance),
    ("cc_scripts_per_once", scripts::per_once),
    ("cc_scripts_user", scripts::user),
    ("cc_scripts_vendor", scripts::vendor),
    ("cc_seed_random", seed_random::handle),
    ("cc_set_hostname", set_hostname::handle),
    ("cc_set_passwords", set_passwords::handle),
    ("cc_snap", snap::handle),
    ("cc_ssh", ssh::handle),
    (
        "cc_ssh_authkey_fingerprints",
        ssh_authkey_fingerprints::handle,
    ),
    ("cc_ssh_import_id", ssh_import_id::handle),
    ("cc_timezone", timezone::handle),
    ("cc_update_etc_hosts", update_etc_hosts::handle),
    ("cc_update_hostname", update_hostname::handle),
    ("cc_users_groups", users_groups::handle),
    ("cc_write_files", write_files::handle),
    ("cc_write_files_deferred", write_files_deferred::handle),
];

/// The body for a module, or `None` if it has not been ported yet.
#[must_use]
pub fn handler(module_name: &str) -> Option<Handler> {
    HANDLERS
        .binary_search_by_key(&module_name, |(name, _)| *name)
        .ok()
        .and_then(|index| HANDLERS.get(index))
        .map(|(_, handler)| *handler)
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

    /// The datasource the other modules' tests run against. Shared because
    /// none of them are *about* the datasource: they want `i-test` to resolve
    /// and nothing else.
    pub(super) fn fixture_datasource() -> Datasource<'static> {
        static EMPTY: std::sync::OnceLock<Object> = std::sync::OnceLock::new();
        let empty = EMPTY.get_or_init(Object::new);
        Datasource {
            class_name: "DataSourceNoCloud",
            dsname: "NoCloud",
            instance_id: "i-test",
            metadata: empty,
            sys_cfg: empty,
            public_keys: &[],
        }
    }

    /// The empty `system_info` a module test uses when it is not about the
    /// distro's own config.
    pub(super) fn no_system_info() -> &'static Object {
        static EMPTY: std::sync::OnceLock<Object> = std::sync::OnceLock::new();
        EMPTY.get_or_init(Object::new)
    }

    /// The distro the other modules' tests run against, matching the
    /// `SimpleNamespace` the differential harness fakes on the Python side.
    pub(super) fn fixture_distro() -> &'static ci_distro::Distro {
        ci_distro::fetch("ubuntu").unwrap()
    }

    #[test]
    fn the_handler_table_is_sorted_so_the_binary_search_holds() {
        let mut sorted: Vec<&str> = HANDLERS.iter().map(|(name, _)| *name).collect();
        let as_written = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(as_written, sorted);
    }

    #[test]
    fn every_ported_module_is_a_module_the_registry_knows() {
        for (name, _) in HANDLERS {
            assert!(
                crate::find(name).is_some(),
                "{name} has a body but is not in the generated registry"
            );
        }
    }

    #[test]
    fn an_unported_module_has_no_body() {
        assert!(handler("cc_write_files").is_some());
        assert!(handler("cc_mounts").is_some());
        assert!(handler("cc_not_a_module").is_none());
    }

    #[test]
    fn shellify_quotes_list_items_and_passes_strings_through() {
        let (script, made) = shellify(&serde_json::json!([
            "echo hi > /tmp/x",
            ["echo", "it's", 5],
            null,
        ]))
        .unwrap();
        assert_eq!(
            script,
            "#!/bin/sh\necho hi > /tmp/x\n'echo' 'it'\\''s' '5'\n"
        );
        assert_eq!(made, 2);
    }

    #[test]
    fn shellify_rejects_what_python_rejects() {
        assert_eq!(
            shellify(&serde_json::json!("echo hi")),
            Err("Input to shellify was type 'str'. Expected list or tuple.".to_owned())
        );
        assert_eq!(
            shellify(&serde_json::json!([5])),
            Err(
                "Unable to shellify type 'int'. Expected list, string, tuple. Got: 5"
                    .to_owned()
            )
        );
    }
}
