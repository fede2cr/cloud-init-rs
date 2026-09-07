//! Version reporting.
//!
//! `cloud-init --version` is parsed by provisioning scripts in the wild, so the
//! output shape must not change: a bare version string and nothing else. What is
//! reported is the *upstream release this build targets for compatibility*, not
//! the crate version; the implementation and its own version are exposed
//! separately so operators can still tell which binary answered.
//!
//! The two are kept apart on purpose. `version_string()` answers "which
//! cloud-init is this", and must stay `26.1` whatever the tag says.
//! `agent()` answers "who is talking to the platform", and must not claim to be
//! upstream Python.

/// Upstream cloud-init release this build aims to be compatible with.
pub const COMPAT_VERSION: &str = "26.1";

/// Implementation name, as reported in `/run/cloud-init/.impl`.
pub const IMPL_NAME: &str = "rust";

/// Version of this implementation.
pub const IMPL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The distribution's package version, when a packager stamped one in.
///
/// Upstream's `version.py` has the same escape hatch, spelled
/// `_DOWNSTREAM_VERSION`, which is why a stock Ubuntu box answers
/// `26.1-0ubuntu3~26.04.1` rather than `26.1`. `packaging/build-deb.sh` sets
/// this to the `.deb` version.
pub const DOWNSTREAM_VERSION: Option<&str> =
    option_env!("CLOUD_INIT_RS_DOWNSTREAM_VERSION");

/// The string printed by `cloud-init --version`.
pub fn version_string() -> String {
    COMPAT_VERSION.to_owned()
}

/// This implementation's own version: the package version once packaged, the
/// crate version otherwise.
pub fn impl_version_string() -> &'static str {
    match DOWNSTREAM_VERSION {
        Some(version) => version,
        None => IMPL_VERSION,
    }
}

/// `util.make_header`: the "cloud-init wrote this" line at the top of a
/// generated file.
///
/// `base` is title-cased, as upstream's `base.title()` does, and the timestamp
/// is the moment of the call — so a file rewritten with identical content
/// still differs from its predecessor by this line.
pub fn make_header(comment_char: char, base: &str) -> String {
    let mut titled = String::new();
    for (i, ch) in base.chars().enumerate() {
        if i == 0 {
            titled.extend(ch.to_uppercase());
        } else {
            titled.extend(ch.to_lowercase());
        }
    }
    format!(
        "{comment_char} {titled} by cloud-init v. {} on {}",
        version_string(),
        crate::time::format_last_update(crate::time::now_epoch())
    )
}

/// How this implementation names itself to a cloud platform, in the `agent=`
/// field of a provisioning report and in `User-Agent`.
///
/// Deliberately `Cloud-Init-rs`, not upstream's `Cloud-Init`: a report Azure
/// keys on should say which implementation produced it (COMPAT deviation 102).
pub fn agent() -> String {
    format!("Cloud-Init-rs/{}", impl_version_string())
}

/// Human-readable build identity, for logs and `--long` output.
pub fn build_string() -> String {
    format!("cloud-init-rs {IMPL_VERSION} (cloud-init {COMPAT_VERSION} compatible)")
}
