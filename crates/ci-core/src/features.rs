//! Port of `cloudinit/features.py`.
//!
//! Upstream prints every boolean feature flag it defines. We only advertise flags
//! whose behaviour this implementation actually provides, because consumers use
//! this list to decide whether a behaviour can be relied on; advertising an
//! unimplemented flag would be worse than advertising nothing.
//!
//! The list grows as phases land — see `docs/COMPAT.md`.

/// Feature flags implemented by this build, in upstream declaration order.
pub const ALL_FEATURES: &[&str] = &[];

pub fn render() -> String {
    let mut out = ALL_FEATURES.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}
