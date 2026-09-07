//! Port of the instance-directory half of `stages.Init`.
//!
//! This is what turns `/var/lib/cloud/instances/<iid>` into "the" instance:
//! `Init.instancify` relinks `/var/lib/cloud/instance`, records which datasource
//! and instance id are in play, and remembers the previous pair so the rest of
//! the boot can tell a re-run from a genuinely new instance.
//!
//! Upstream reads the id and the datasource's `repr` off `self.ds`; both arrive
//! as arguments here until datasources land in Phase 3.

use std::io;
use std::path::{Path, PathBuf};

use ci_config::Object;
use ci_sys::atomic::{write_file, WriteOptions};

use crate::paths::{Lookup, Paths};

/// `stages.NO_PREVIOUS_INSTANCE_ID`.
pub const NO_PREVIOUS_INSTANCE_ID: &str = "NO_PREVIOUS_INSTANCE_ID";

/// Directories `_get_instance_subdirs` guarantees inside the instance directory.
const INSTANCE_SUBDIRS: [&str; 3] = ["handlers", "scripts", "sem"];

/// What `Init._reflect_cur_instance` found and recorded.
#[derive(Debug, Clone)]
pub struct Reflected {
    /// The instance id now in force.
    pub iid: String,
    /// The id from the last boot, or [`NO_PREVIOUS_INSTANCE_ID`].
    pub previous_iid: String,
    /// The datasource now in force, as `"<type>: <repr>"`.
    pub datasource: String,
    /// The datasource from the last boot, defaulting to the current one.
    pub previous_datasource: String,
    /// Whether the instance symlink already pointed at the right directory.
    pub already_instancified: bool,
    /// Whether upstream would call `_reset()` and re-read the configuration.
    pub reload: bool,
}

impl Reflected {
    /// `Init.is_new_instance`.
    ///
    /// Answered from the id captured before the relink, because [`reflect`] has
    /// since overwritten the file it came from; upstream memoises
    /// `_previous_iid` for the same reason.
    #[must_use]
    pub fn is_new_instance(&self) -> bool {
        self.previous_iid == NO_PREVIOUS_INSTANCE_ID || self.previous_iid != self.iid
    }
}

/// `Init.previous_iid`, read fresh from disk.
///
/// Only meaningful before [`reflect`] runs.
#[must_use]
pub fn previous_iid(paths: &Paths) -> String {
    read_stripped(&paths.data_dir().join("instance-id"))
        .filter(|iid| !iid.is_empty())
        .unwrap_or_else(|| NO_PREVIOUS_INSTANCE_ID.to_owned())
}

/// `Paths._get_ipath()` with no name: `<cloud_dir>/instances/<iid>`.
#[must_use]
pub fn instance_dir(paths: &Paths, iid: &str) -> PathBuf {
    paths.instances_dir().join(iid.replace('/', "_"))
}

/// `Init.instancify` — point the instance link at `iid` and record the change.
///
/// `datasource` is upstream's `"%s: %s" % (obj_name(ds), ds)`. The state tree
/// must already exist, as it does after `_initialize_filesystem`.
pub fn reflect(
    paths: &Paths,
    iid: &str,
    datasource: &str,
    cfg: &Object,
) -> io::Result<Reflected> {
    let idir = instance_dir(paths, iid);
    let link = paths.instance_link();
    let already_instancified = points_at(&link, &idir);
    if !already_instancified {
        // `del_file` then `sym_link`, not an atomic swap: upstream leaves the
        // link missing in between (COMPAT.md B33).
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        ci_sys::path::sym_link(&idir, &link, false)?;
    }

    for sub in INSTANCE_SUBDIRS {
        ci_sys::path::ensure_dir(idir.join(sub), 0o755)?;
    }

    let data = paths.data_dir();
    ci_sys::path::ensure_dir(&data, 0o755)?;

    let ds_file = idir.join("datasource");
    let previous_datasource = read_stripped(&ds_file)
        .filter(|previous| !previous.is_empty())
        .unwrap_or_else(|| datasource.to_owned());
    write_line(&ds_file, datasource)?;
    write_line(&data.join("previous-datasource"), &previous_datasource)?;

    // Read before the write below replaces it.
    let previous_iid = previous_iid(paths);
    write_line(&data.join("instance-id"), iid)?;
    write_line(&paths.run_path(Lookup::InstanceId), iid)?;
    write_line(&data.join("previous-instance-id"), &previous_iid)?;

    write_to_cache(paths, cfg)?;

    let reload = !(already_instancified && previous_datasource == datasource);
    Ok(Reflected {
        iid: iid.to_owned(),
        previous_iid,
        datasource: datasource.to_owned(),
        previous_datasource,
        already_instancified,
        reload,
    })
}

/// `Init._write_to_cache`, minus the pickle upstream also stores there.
///
/// The empty `manual-clean` marker is the part that has a consumer outside the
/// Python process: `ds-identify` reads it to decide whether the cache is being
/// cleaned by hand.
fn write_to_cache(paths: &Paths, cfg: &Object) -> io::Result<()> {
    if ci_config::option::get_bool(cfg, "manual_cache_clean", false) {
        write_file(
            paths.instance_path(Lookup::ManualCleanMarker),
            b"",
            WriteOptions::PUBLIC,
        )?;
    }
    Ok(())
}

/// `Path(link).resolve().absolute() == Path(target).absolute()`.
///
/// The asymmetry is upstream's: the link is fully resolved but the target is
/// not, so a symlinked component anywhere in `cloud_dir` makes this always false
/// (COMPAT.md B33). Reproduced rather than fixed.
fn points_at(link: &Path, target: &Path) -> bool {
    let Ok(resolved) = std::fs::canonicalize(link) else {
        // Python resolves the surviving parents instead of failing, but the link
        // and the instance directory are different names under the same parent,
        // so a missing link can never compare equal either way.
        return false;
    };
    resolved == *target
}

fn read_stripped(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

fn write_line(path: &Path, value: &str) -> io::Result<()> {
    write_file(path, format!("{value}\n"), WriteOptions::PUBLIC)
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

    fn paths(root: &Path) -> Paths {
        let paths = Paths {
            cloud_dir: root.join("var/lib/cloud"),
            run_dir: root.join("run/cloud-init"),
            ..Paths::default()
        };
        // Stands in for `_initialize_filesystem`, which always runs first.
        std::fs::create_dir_all(paths.instances_dir()).unwrap();
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        paths
    }

    fn empty() -> Object {
        Object::new()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn the_first_boot_has_no_previous_instance_and_links_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());

        let seen =
            reflect(&paths, "i-0001", "DataSourceNone: DataSourceNone", &empty())
                .unwrap();

        assert_eq!(seen.previous_iid, NO_PREVIOUS_INSTANCE_ID);
        assert!(seen.is_new_instance());
        assert!(!seen.already_instancified);
        assert!(seen.reload);
        assert_eq!(
            std::fs::read_link(paths.instance_link()).unwrap(),
            instance_dir(&paths, "i-0001")
        );
        assert_eq!(read(&paths.data_dir().join("instance-id")), "i-0001\n");
        assert_eq!(read(&paths.run_path(Lookup::InstanceId)), "i-0001\n");
    }

    #[test]
    fn a_previous_datasource_defaults_to_the_current_one_on_a_first_boot() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());

        let seen = reflect(&paths, "i-0001", "DataSourceNone: x", &empty()).unwrap();

        assert_eq!(seen.previous_datasource, "DataSourceNone: x");
        assert_eq!(
            read(&paths.data_dir().join("previous-datasource")),
            "DataSourceNone: x\n"
        );
    }

    #[test]
    fn rebooting_the_same_instance_keeps_the_link_and_skips_the_reload() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        reflect(&paths, "i-0001", "DataSourceNone: x", &empty()).unwrap();

        let seen = reflect(&paths, "i-0001", "DataSourceNone: x", &empty()).unwrap();

        assert!(seen.already_instancified);
        assert!(!seen.reload);
        assert!(!seen.is_new_instance());
        assert_eq!(seen.previous_iid, "i-0001");
    }

    #[test]
    fn a_changed_instance_id_relinks_and_records_the_one_it_replaced() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        reflect(&paths, "i-0001", "DataSourceNone: x", &empty()).unwrap();

        let seen = reflect(&paths, "i-0002", "DataSourceNone: x", &empty()).unwrap();

        assert!(seen.is_new_instance());
        assert!(seen.reload);
        assert_eq!(seen.previous_iid, "i-0001");
        assert_eq!(
            read(&paths.data_dir().join("previous-instance-id")),
            "i-0001\n"
        );
        assert_eq!(
            std::fs::read_link(paths.instance_link()).unwrap(),
            instance_dir(&paths, "i-0002")
        );
    }

    #[test]
    fn the_same_instance_on_a_different_datasource_forces_a_reload() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        reflect(&paths, "i-0001", "DataSourceNoCloud: x", &empty()).unwrap();

        let seen = reflect(&paths, "i-0001", "DataSourceEc2: y", &empty()).unwrap();

        assert!(seen.already_instancified);
        assert!(seen.reload);
        assert_eq!(seen.previous_datasource, "DataSourceNoCloud: x");
        assert!(!seen.is_new_instance());
    }

    #[test]
    fn an_instance_id_with_a_slash_cannot_escape_the_instances_directory() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());

        let seen = reflect(&paths, "../../etc/i-0001", "ds: x", &empty()).unwrap();

        assert_eq!(seen.iid, "../../etc/i-0001");
        assert_eq!(
            instance_dir(&paths, "../../etc/i-0001"),
            paths.instances_dir().join(".._.._etc_i-0001")
        );
        assert!(paths.instances_dir().join(".._.._etc_i-0001").is_dir());
    }

    #[test]
    fn the_manual_clean_marker_is_written_only_when_the_config_asks_for_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let marker = paths.instance_path(Lookup::ManualCleanMarker);

        reflect(&paths, "i-0001", "ds: x", &empty()).unwrap();
        assert!(!marker.exists());

        let mut cfg = Object::new();
        cfg.insert("manual_cache_clean".to_owned(), true.into());
        reflect(&paths, "i-0001", "ds: x", &cfg).unwrap();
        assert_eq!(read(&marker), "");
    }

    #[test]
    fn the_subdirectories_the_handlers_and_semaphores_need_are_created() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());

        reflect(&paths, "i-0001", "ds: x", &empty()).unwrap();

        let idir = instance_dir(&paths, "i-0001");
        for sub in INSTANCE_SUBDIRS {
            assert!(idir.join(sub).is_dir(), "{sub}");
        }
    }

    #[test]
    fn a_symlinked_cloud_dir_defeats_the_already_instancified_check() {
        let root = tempfile::tempdir().unwrap();
        // Stand in for a `/var` that is itself a symlink, which is all it takes.
        std::fs::create_dir_all(root.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.path().join("var")).unwrap();
        let paths = paths(root.path());
        reflect(&paths, "i-0001", "ds: x", &empty()).unwrap();

        let seen = reflect(&paths, "i-0001", "ds: x", &empty()).unwrap();

        assert!(!seen.already_instancified);
        assert!(seen.reload);
    }
}
