//! Port of `cloudinit/features.py`.
//!
//! Upstream prints every boolean feature flag it defines. We only advertise flags
//! whose behaviour this implementation actually provides, because consumers use
//! this list to decide whether a behaviour can be relied on; advertising an
//! unimplemented flag would be worse than advertising nothing.
//!
//! The list grows as phases land — see `docs/COMPAT.md`.

/// `APT_DEB822_SOURCE_LIST_FILE`: on Debian and Ubuntu, `cc_apt_configure`
/// writes a deb822 `/etc/apt/sources.list.d/<distro>.sources` rather than
/// `/etc/apt/sources.list`.
pub const APT_DEB822_SOURCE_LIST_FILE: bool = true;

/// `ALLOW_EC2_MIRRORS_ON_NON_AWS_INSTANCE_TYPES`: whether an EC2-shaped
/// availability zone may be turned into an `ec2_region` substitution on a
/// platform that is not EC2. Upstream ships this off, and it is not in
/// [`ALL_FEATURES`] because only the flags that are on are advertised.
pub const ALLOW_EC2_MIRRORS_ON_NON_AWS_INSTANCE_TYPES: bool = false;

/// Feature flags implemented by this build, in upstream declaration order.
///
/// Every name here is advertised as `true`, so a flag only belongs in the list
/// while its constant above is `true`.
pub const ALL_FEATURES: &[&str] = &["APT_DEB822_SOURCE_LIST_FILE"];

pub fn render() -> String {
    let mut out = ALL_FEATURES.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}
