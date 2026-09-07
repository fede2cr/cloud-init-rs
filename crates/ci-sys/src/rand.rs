//! Randomness from the kernel.

use std::fs::File;
use std::io::Read as _;

/// A random `u64`, as `random.randrange(sys.maxsize)` supplies upstream.
///
/// `/dev/urandom` is the whole source: a userspace generator would need a seed
/// from here anyway. If it cannot be read the clock stands in, which is weaker
/// but only ever decides a MIME boundary, never a secret.
#[must_use]
pub fn u64() -> u64 {
    let mut bytes = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_ne_bytes(bytes);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos().into())
}

/// Fill `buf` from the kernel, or fail.
///
/// The counterpart to [`u64`] for the one caller whose output is a secret: a
/// generated password must never fall back to the clock, so a failure here is
/// reported rather than papered over.
pub fn fill(buf: &mut [u8]) -> std::io::Result<()> {
    File::open("/dev/urandom")?.read_exact(buf)
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

    #[test]
    fn two_draws_differ() {
        assert_ne!(u64(), u64());
    }
}
