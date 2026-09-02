//! Hardened OS primitives shared by every cloud-init-rs crate.
//!
//! Nothing in the tree is allowed to call [`std::fs::write`] or
//! [`std::process::Command`] directly; everything goes through this crate so that
//! atomicity, permissions and process hygiene are enforced in exactly one place.

pub mod atomic;
pub mod path;
pub mod subp;

pub use atomic::{write_file, WriteOptions};
pub use subp::{run, Output, Subp};

/// Real UID of this process.
///
/// Read from `/proc/self` rather than `getuid(2)` to keep the crate free of
/// `unsafe` and of a libc dependency. On failure this reports a non-root UID, so
/// callers fail closed and treat data as unreadable rather than privileged.
pub fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map_or(u32::MAX, |m| m.uid())
}

pub fn is_root() -> bool {
    current_uid() == 0
}

/// A random 64-bit value from the kernel CSPRNG.
///
/// Upstream generates MIME boundaries with `random.randrange`, a Mersenne
/// Twister whose state is recoverable from its output. The email module's
/// collision check makes that safe today, so this is hardening rather than a
/// fix; it also avoids depending on a userspace PRNG for anything else.
pub fn random_u64() -> std::io::Result<u64> {
    use std::io::Read;

    let mut buf = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(u64::from_ne_bytes(buf))
}
