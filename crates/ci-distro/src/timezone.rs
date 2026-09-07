//! `Distro.set_timezone`: pointing the system at a zone file.
//!
//! Setting a timezone on Linux means two things that can disagree. glibc reads
//! `/etc/localtime`, so that is what actually decides what `date` prints;
//! Debian's tooling also keeps the *name* in `/etc/timezone` so that a package
//! reconfigure knows what was asked for. Upstream's variants differ mostly in
//! which of those two they bother with, and in whether `/etc/localtime` ends
//! up a symlink or a copy.
//!
//! The copy branch is the interesting one. `distros.set_etc_timezone` only
//! links when `/etc/localtime` is already a link or is missing; if it is a
//! real file — which is what a `tzdata` reconfigure on some systems leaves
//! behind — it copies the zone file over it instead, so the system keeps
//! working but stops tracking `tzdata` updates. Reproduced rather than
//! improved.
//!
//! Split into [`plan`] and [`run`] like the modules that shell out, for the
//! same reason: the decision depends on three facts about the filesystem, and
//! a plan that names them can be checked against upstream without touching a
//! live `/etc`.

use std::path::Path;

use crate::{Distro, TimezoneWriter, TZ_ZONE_DIR};

const SOURCE: &str = "distros/__init__.py";

/// The state of `/etc/localtime`, which is what decides between a symlink and
/// a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTime {
    /// `os.path.islink` is true. Deleted and re-linked.
    Symlink,
    /// It exists and is not a link. Overwritten by a *copy*.
    Regular,
    /// It does not exist. Linked.
    Absent,
}

impl LocalTime {
    /// Read the state off a path, following upstream's order: `islink` first,
    /// so a dangling symlink is a link and not an absence.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        if path.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) {
            Self::Symlink
        } else if path.exists() {
            Self::Regular
        } else {
            Self::Absent
        }
    }
}

/// One effect of `set_timezone`, with paths as upstream spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `util.write_file(tz_conf, str(tz).rstrip() + "\n")`.
    WriteName { path: String, contents: String },
    /// `util.del_file`, which ignores a file that is not there.
    Remove { path: String },
    /// `os.symlink(target, path)`.
    ///
    /// `target` is the zone file's absolute path as upstream computes it,
    /// unrooted even when the module is pointed at a fixture directory —
    /// upstream writes that string into the link verbatim, and a rooted
    /// target would not be the same link.
    Link { target: String, path: String },
    /// `util.copy`, i.e. `shutil.copy`: contents and permission bits.
    Copy { from: String, to: String },
}

/// `Distro.set_timezone`, as a plan.
///
/// `zone_file_exists` is `os.path.isfile(tz_file)`, and `localtime` the state
/// of the link this distro manages; both are passed in so the decision stays
/// a pure function of them.
///
/// # Errors
/// The `IOError("Invalid timezone %s, no file found at %s")` that
/// `_find_tz_file` raises for a name with no zone file — which is the only
/// validation a timezone gets, so a name that exists but is nonsense is
/// applied without comment. Also a variant this port does not implement.
pub fn plan(
    distro: &Distro,
    tz: &str,
    zone_file_exists: bool,
    localtime: LocalTime,
    systemd: bool,
) -> Result<Vec<Step>, String> {
    let tz_file = zone_file(tz);
    if !zone_file_exists {
        return Err(format!("Invalid timezone {tz}, no file found at {tz_file}"));
    }
    // `set_etc_timezone`'s defaults, which are function arguments upstream
    // rather than class attributes, so the classes that use it have no
    // `tz_local_fn` of their own.
    let tz_local = distro.tz_local_fn.unwrap_or("/etc/localtime");

    match distro.timezone_writer {
        TimezoneWriter::EtcTimezone => {
            let mut steps = vec![Step::WriteName {
                path: "/etc/timezone".to_owned(),
                contents: format!("{}\n", rstrip(tz)),
            }];
            steps.extend(relink(&tz_file, tz_local, localtime));
            Ok(steps)
        }
        // No `/etc/timezone`, and no copy branch: aosc replaces the link
        // whatever was there, so a `/etc/localtime` that was a real file
        // becomes a link.
        TimezoneWriter::Aosc => Ok(vec![
            Step::Remove {
                path: tz_local.to_owned(),
            },
            Step::Link {
                target: tz_file,
                path: tz_local.to_owned(),
            },
        ]),
        TimezoneWriter::Rhel | TimezoneWriter::OpenSuse if systemd => Ok(vec![
            Step::Remove {
                path: tz_local.to_owned(),
            },
            Step::Link {
                target: tz_file,
                path: tz_local.to_owned(),
            },
        ]),
        TimezoneWriter::Rhel | TimezoneWriter::OpenSuse => Err(format!(
            "update_sysconfig_file is not ported; this distro needs it to set \
             a timezone without systemd (would have written {} to {})",
            sysconfig_key(distro.timezone_writer),
            distro.clock_conf_fn.unwrap_or("/etc/sysconfig/clock"),
        )),
        TimezoneWriter::Bsd => {
            Err("the BSD set_timezone is not ported; this distro needs it".to_owned())
        }
    }
}

/// `set_etc_timezone`'s link-or-copy tail.
fn relink(tz_file: &str, tz_local: &str, localtime: LocalTime) -> Vec<Step> {
    match localtime {
        LocalTime::Symlink => vec![
            Step::Remove {
                path: tz_local.to_owned(),
            },
            Step::Link {
                target: tz_file.to_owned(),
                path: tz_local.to_owned(),
            },
        ],
        LocalTime::Absent => vec![Step::Link {
            target: tz_file.to_owned(),
            path: tz_local.to_owned(),
        }],
        // A real file is copied over, not replaced by a link: the system keeps
        // the right time but stops following `tzdata` updates.
        LocalTime::Regular => vec![Step::Copy {
            from: tz_file.to_owned(),
            to: tz_local.to_owned(),
        }],
    }
}

/// `os.path.join(self.tz_zone_dir, str(tz))`, including the case that makes it
/// not a join at all.
#[must_use]
pub fn zone_file(tz: &str) -> String {
    if tz.starts_with('/') {
        // `os.path.join` discards everything before an absolute component, so
        // `timezone: /etc/shadow` names that file directly. It still has to
        // exist and still gets linked into `/etc/localtime`, which reads it as
        // a zone file and fails; it is not a way to write anything.
        return tz.to_owned();
    }
    format!("{TZ_ZONE_DIR}/{tz}")
}

/// Which sysconfig key the non-systemd branch would have written. Only used in
/// the message that says it is not ported.
fn sysconfig_key(writer: TimezoneWriter) -> &'static str {
    match writer {
        TimezoneWriter::OpenSuse => "TIMEZONE",
        _ => "ZONE",
    }
}

/// Python's `str.rstrip()`.
fn rstrip(text: &str) -> &str {
    text.trim_end_matches(ci_core::pystr::is_space)
}

/// Carry out a plan under `root`.
///
/// # Errors
/// The first step that fails. Upstream lets these propagate out of the module,
/// and so does this.
pub fn run(
    steps: &[Step],
    root: &Path,
    log: &mut ci_log::Logger,
) -> Result<(), String> {
    for step in steps {
        match step {
            Step::WriteName { path, contents } => {
                let target = rooted(root, path);
                ci_sys::atomic::write_file(
                    &target,
                    contents.as_bytes(),
                    ci_sys::atomic::WriteOptions {
                        mode: 0o644,
                        ..ci_sys::atomic::WriteOptions::default()
                    },
                )
                .map_err(|error| format!("{}: {error}", target.display()))?;
            }
            Step::Remove { path } => {
                let target = rooted(root, path);
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    // `util.del_file` catches ENOENT and logs the rest, but
                    // every caller here has just checked, so anything else is
                    // real.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!("{}: {error}", target.display()));
                    }
                }
            }
            Step::Link { target, path } => {
                let link = rooted(root, path);
                log.debug(
                    SOURCE,
                    &format!("Creating symbolic link from '{path}' => '{target}'"),
                );
                std::os::unix::fs::symlink(target, &link)
                    .map_err(|error| format!("{}: {error}", link.display()))?;
            }
            Step::Copy { from, to } => {
                let (from, to) = (rooted(root, from), rooted(root, to));
                std::fs::copy(&from, &to)
                    .map_err(|error| format!("{}: {error}", to.display()))?;
            }
        }
    }
    Ok(())
}

fn rooted(root: &Path, path: &str) -> std::path::PathBuf {
    root.join(path.trim_start_matches('/'))
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
    use crate::fetch;

    fn distro(name: &str) -> &'static Distro {
        fetch(name).unwrap()
    }

    fn plan_for(name: &str, tz: &str, localtime: LocalTime) -> Vec<Step> {
        plan(distro(name), tz, true, localtime, true).unwrap()
    }

    #[test]
    fn debian_writes_the_name_and_links_the_zone_file() {
        assert_eq!(
            plan_for("ubuntu", "Europe/Madrid", LocalTime::Symlink),
            vec![
                Step::WriteName {
                    path: "/etc/timezone".to_owned(),
                    contents: "Europe/Madrid\n".to_owned(),
                },
                Step::Remove {
                    path: "/etc/localtime".to_owned(),
                },
                Step::Link {
                    target: "/usr/share/zoneinfo/Europe/Madrid".to_owned(),
                    path: "/etc/localtime".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_missing_localtime_is_linked_without_being_removed_first() {
        let steps = plan_for("debian", "UTC", LocalTime::Absent);
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[1], Step::Link { .. }));
    }

    #[test]
    fn a_real_localtime_is_copied_over_rather_than_relinked() {
        // The branch that quietly stops the system tracking tzdata.
        let steps = plan_for("debian", "UTC", LocalTime::Regular);
        assert_eq!(
            steps[1],
            Step::Copy {
                from: "/usr/share/zoneinfo/UTC".to_owned(),
                to: "/etc/localtime".to_owned(),
            }
        );
    }

    #[test]
    fn aosc_always_links_and_never_writes_the_name() {
        // Even for a `/etc/localtime` that is a real file, where the shared
        // implementation would have copied.
        for state in [LocalTime::Symlink, LocalTime::Regular, LocalTime::Absent] {
            assert_eq!(
                plan_for("aosc", "UTC", state),
                vec![
                    Step::Remove {
                        path: "/etc/localtime".to_owned(),
                    },
                    Step::Link {
                        target: "/usr/share/zoneinfo/UTC".to_owned(),
                        path: "/etc/localtime".to_owned(),
                    },
                ],
                "{state:?}"
            );
        }
    }

    #[test]
    fn rhel_under_systemd_links_but_writes_no_etc_timezone() {
        let steps = plan_for("rhel", "UTC", LocalTime::Symlink);
        assert_eq!(steps.len(), 2);
        assert!(!steps.iter().any(|s| matches!(s, Step::WriteName { .. })));
    }

    #[test]
    fn rhel_without_systemd_is_not_ported() {
        let error =
            plan(distro("centos"), "UTC", true, LocalTime::Symlink, false).unwrap_err();
        assert!(error.contains("not ported"), "{error}");
        assert!(error.contains("ZONE"), "{error}");
    }

    #[test]
    fn opensuse_without_systemd_names_its_own_sysconfig_key() {
        let error =
            plan(distro("sles"), "UTC", true, LocalTime::Symlink, false).unwrap_err();
        assert!(error.contains("TIMEZONE"), "{error}");
    }

    #[test]
    fn an_unknown_zone_is_the_only_validation_there_is() {
        let error = plan(
            distro("ubuntu"),
            "Mars/Olympus",
            false,
            LocalTime::Absent,
            true,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Invalid timezone Mars/Olympus, no file found at \
             /usr/share/zoneinfo/Mars/Olympus"
        );
    }

    #[test]
    fn an_absolute_timezone_escapes_the_zone_directory() {
        // `os.path.join` drops the prefix. Worth a test because it looks like
        // a path traversal and is not quite one: the file still has to exist,
        // and it is only ever read.
        assert_eq!(zone_file("/etc/shadow"), "/etc/shadow");
        assert_eq!(
            zone_file("../../etc/shadow"),
            "/usr/share/zoneinfo/../../etc/shadow"
        );
    }

    #[test]
    fn trailing_whitespace_is_stripped_from_the_name_but_not_from_the_path() {
        // `set_etc_timezone` rstrips only what it writes to `/etc/timezone`;
        // `_find_tz_file` joined the unstripped name, so the two disagree.
        let steps = plan_for("ubuntu", "UTC  ", LocalTime::Absent);
        assert_eq!(
            steps[0],
            Step::WriteName {
                path: "/etc/timezone".to_owned(),
                contents: "UTC\n".to_owned(),
            }
        );
        assert_eq!(
            steps[1],
            Step::Link {
                target: "/usr/share/zoneinfo/UTC  ".to_owned(),
                path: "/etc/localtime".to_owned(),
            }
        );
    }

    #[test]
    fn every_distro_resolves_to_a_writer() {
        for distro in crate::DISTROS {
            let result = plan(distro, "UTC", true, LocalTime::Absent, true);
            match distro.timezone_writer {
                TimezoneWriter::Bsd => assert!(result.is_err(), "{}", distro.name),
                _ => assert!(result.is_ok(), "{}: {result:?}", distro.name),
            }
        }
    }

    #[test]
    fn run_links_under_a_fixture_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        let mut log = ci_log::Logger::silent();
        let steps = plan_for("ubuntu", "UTC", LocalTime::Absent);
        run(&steps, root, &mut log).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("etc/timezone")).unwrap(),
            "UTC\n"
        );
        assert_eq!(
            std::fs::read_link(root.join("etc/localtime")).unwrap(),
            Path::new("/usr/share/zoneinfo/UTC"),
        );
    }
}
