//! `util.decomp_gzip`.
//!
//! Lives here rather than in one of its callers because both the user-data
//! pipeline and `write_files` need it, and a second implementation is a second
//! place for the size cap to be forgotten.

use std::io::Read as _;

/// Refuse to inflate more than this. A gzip stream is attacker-supplied on
/// every path that reaches here — user-data, and `write_files` content — and
/// the ratio is unbounded, so the cap is the only thing between a 1 KiB blob
/// and an out-of-memory kill.
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

/// Inflate a gzip stream, or fail.
///
/// This is upstream's `quiet=False` behaviour. The `quiet=True` callers want
/// the original bytes back on failure, which is `decompress(data).unwrap_or_else(|_| data.to_vec())`
/// at the call site — spelled out there rather than offered as a second
/// function, because "silently returns its input" is worth seeing.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(MAX_DECOMPRESSED)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}
