//! Version reporting.
//!
//! `cloud-init --version` is parsed by provisioning scripts in the wild, so the
//! output shape must not change: a bare version string and nothing else. What is
//! reported is the *upstream release this build targets for compatibility*, not
//! the crate version; the implementation and its own version are exposed
//! separately so operators can still tell which binary answered.

/// Upstream cloud-init release this build aims to be compatible with.
pub const COMPAT_VERSION: &str = "26.1";

/// Implementation name, as reported in `/run/cloud-init/.impl`.
pub const IMPL_NAME: &str = "rust";

/// Version of this implementation.
pub const IMPL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The string printed by `cloud-init --version`.
pub fn version_string() -> String {
    COMPAT_VERSION.to_owned()
}

/// Human-readable build identity, for logs and `--long` output.
pub fn build_string() -> String {
    format!("cloud-init-rs {IMPL_VERSION} (cloud-init {COMPAT_VERSION} compatible)")
}
