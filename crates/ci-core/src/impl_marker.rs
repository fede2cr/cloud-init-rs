//! `/run/cloud-init/.impl` — which implementation is driving this boot.
//!
//! The two implementations share `/var/lib/cloud` and `/run/cloud-init`, and
//! `update-alternatives` lets an admin move between them at any moment — which
//! includes the moment between `init-local` and `init`, or between `init` and
//! `modules:config`. Nothing in either implementation would notice. The four
//! stages of one boot are not independent: the later ones read state the earlier
//! ones wrote, so half a boot's worth of Python state finished by Rust (or the
//! reverse) is a machine that is neither implementation's tested configuration.
//!
//! So the first stage of a boot stamps this file and every later stage checks
//! it. A mismatch is a hard error, not a warning: a stage that refuses to run
//! leaves a machine that is obviously unfinished, which an operator can see and
//! fix, whereas a stage that continues leaves one that looks finished and is
//! not. PLAN.md §6.7.
//!
//! The format is deliberately trivial `key=value` lines rather than JSON: the
//! Python implementation would have to grow a reader for this too, and the
//! cheaper that is to write the likelier it is to happen (a Phase 9 ask). Any
//! key a reader does not recognise is ignored, so the file can grow fields
//! without the older reader treating a newer boot as foreign.
//!
//! It lives in `/run`, so it is empty again on the next boot by construction —
//! the check is "within one boot", and nothing has to clean up after it.

use std::fmt;
use std::path::Path;

/// Version of the on-disk state layout under `/var/lib/cloud`.
///
/// Bumped when a change makes state written by an older build unsafe for a
/// newer one to continue from. Distinct from both `COMPAT_VERSION` and the
/// package version: those describe the code, this describes the data.
pub const STATE_SCHEMA: u32 = 1;

/// A parsed marker file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// `rust` here, `python` for upstream.
    pub implementation: String,
    /// The implementation's own version, for diagnostics only. Never compared:
    /// an upgrade mid-boot is not the failure this guards against.
    pub version: String,
    pub state_schema: u32,
}

impl fmt::Display for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} (state schema {})",
            self.implementation, self.version, self.state_schema
        )
    }
}

/// What a stage found when it claimed the boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// No marker was there; this stage wrote one.
    Stamped,
    /// A marker was there and it is ours.
    Matched,
    /// A marker was there and it belongs to something else. The stage must not
    /// run.
    Foreign(Marker),
    /// The marker exists but could not be read or parsed.
    ///
    /// Treated as foreign by [`Self::is_fatal`]: an unreadable marker is
    /// exactly as informative about who ran the earlier stages as a foreign
    /// one, which is to say not at all.
    Unreadable(String),
}

impl Claim {
    /// Whether the stage must refuse to run.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Foreign(_) | Self::Unreadable(_))
    }

    /// The message to log and to record in `status.json` when it is fatal.
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Stamped | Self::Matched => None,
            Self::Foreign(other) => Some(format!(
                "Refusing to run: this boot was started by {other}, but this is \
                 {} {} (state schema {STATE_SCHEMA}). Finish the boot with the \
                 implementation that started it, or reboot after selecting one.",
                crate::version::IMPL_NAME,
                crate::version::impl_version_string(),
            )),
            Self::Unreadable(why) => Some(format!(
                "Refusing to run: cannot tell which implementation started this \
                 boot: {why}"
            )),
        }
    }
}

/// Render the marker this build writes.
pub fn render() -> String {
    format!(
        "impl={}\nversion={}\nstate_schema={STATE_SCHEMA}\n",
        crate::version::IMPL_NAME,
        crate::version::impl_version_string(),
    )
}

/// Parse a marker file's contents.
///
/// `None` when a required field is missing or `state_schema` is not a number:
/// the file is ours and short, so anything malformed means something else wrote
/// it, which is the case this whole module exists to catch.
pub fn parse(text: &str) -> Option<Marker> {
    let mut implementation = None;
    let mut version = None;
    let mut state_schema = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        match key.trim() {
            "impl" => implementation = Some(value.trim().to_owned()),
            "version" => version = Some(value.trim().to_owned()),
            "state_schema" => state_schema = Some(value.trim().parse().ok()?),
            // Forward compatibility: a newer writer may add fields, and that
            // alone does not make the boot foreign.
            _ => {}
        }
    }
    Some(Marker {
        implementation: implementation?,
        version: version?,
        state_schema: state_schema?,
    })
}

/// Claim the boot for this implementation.
///
/// `first_stage` is true for `init-local`, which is the stage that begins a
/// boot and therefore overwrites any marker rather than comparing against it.
/// Every later stage compares.
///
/// A marker that is absent for a later stage is *not* an error. A boot can
/// legitimately start at `init` (the local stage is skipped on a datasource
/// that does not need it, and an operator can run one stage by hand), and
/// inventing a failure there would break working setups to catch nothing:
/// there is no earlier writer to disagree with.
pub fn claim(path: &Path, first_stage: bool) -> Claim {
    if !first_stage {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let Some(marker) = parse(&text) else {
                    return Claim::Unreadable(format!(
                        "{} is malformed",
                        path.display()
                    ));
                };
                return if marker.implementation == crate::version::IMPL_NAME
                    && marker.state_schema == STATE_SCHEMA
                {
                    Claim::Matched
                } else {
                    Claim::Foreign(marker)
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Claim::Unreadable(format!("{}: {e}", path.display()));
            }
        }
    }

    match write(path) {
        Ok(()) => Claim::Stamped,
        // Failing to *write* the marker is not fatal. The marker is a guard
        // against a mid-boot switch, not a prerequisite for booting, and a
        // read-only or full `/run` is already going to produce louder errors
        // than this one.
        Err(e) => Claim::Unreadable(format!("{}: {e}", path.display())),
    }
}

/// Write the marker, replacing whatever was there.
fn write(path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Mode explicitly rather than by umask: the contract says 0644 and this is
    // the only writer.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(render().as_bytes())
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

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ci-impl-marker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(".impl")
    }

    #[test]
    fn what_is_written_is_what_is_parsed() {
        let marker = parse(&render()).unwrap();
        assert_eq!(marker.implementation, "rust");
        assert_eq!(marker.state_schema, STATE_SCHEMA);
        assert_eq!(marker.version, crate::version::impl_version_string());
    }

    #[test]
    fn unknown_keys_do_not_make_a_boot_foreign() {
        let marker =
            parse("impl=rust\nversion=9\nstate_schema=1\nsomething_new=x\n# comment\n")
                .unwrap();
        assert_eq!(marker.implementation, "rust");
    }

    #[test]
    fn a_missing_field_is_malformed() {
        assert!(parse("impl=rust\nversion=9\n").is_none());
        assert!(parse("impl=rust\nversion=9\nstate_schema=later\n").is_none());
        assert!(parse("impl rust\n").is_none());
    }

    #[test]
    fn the_first_stage_stamps_over_whatever_was_there() {
        let path = temp();
        std::fs::write(&path, "impl=python\nversion=26.1\nstate_schema=1\n").unwrap();

        assert_eq!(claim(&path, true), Claim::Stamped);
        assert_eq!(
            parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .implementation,
            "rust"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_later_stage_matches_its_own_marker_and_rejects_a_foreign_one() {
        let path = temp();

        // Absent: stamped, not an error — a boot may start at `init`.
        let _ = std::fs::remove_file(&path);
        assert_eq!(claim(&path, false), Claim::Stamped);

        // Ours: fine.
        let claimed = claim(&path, false);
        assert_eq!(claimed, Claim::Matched);
        assert!(!claimed.is_fatal());
        assert!(claimed.message().is_none());

        // Someone else's: fatal, and the message names both sides.
        std::fs::write(&path, "impl=python\nversion=26.1\nstate_schema=1\n").unwrap();
        let claimed = claim(&path, false);
        assert!(claimed.is_fatal());
        let message = claimed.message().unwrap();
        assert!(message.contains("python 26.1"), "{message}");
        assert!(message.contains("rust"), "{message}");

        // Same implementation, incompatible state: also fatal.
        std::fs::write(&path, "impl=rust\nversion=0.1.0\nstate_schema=99\n").unwrap();
        assert!(claim(&path, false).is_fatal());

        // Garbage: fatal, because it is equally uninformative.
        std::fs::write(&path, "not a marker\n").unwrap();
        assert!(claim(&path, false).is_fatal());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_marker_is_written_with_the_mode_the_contract_declares() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = temp();
        let _ = std::fs::remove_file(&path);
        assert_eq!(claim(&path, true), Claim::Stamped);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644, "{mode:04o}");

        let declared = crate::layout::contract()
            .unwrap()
            .into_iter()
            .find(|e| e.path == "/run/cloud-init/.impl")
            .expect("the contract describes the marker");
        assert_eq!(declared.mode, 0o644);

        let _ = std::fs::remove_file(&path);
    }
}
