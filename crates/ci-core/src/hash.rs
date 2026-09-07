//! `util.hash_blob`.
//!
//! MD5 only, and only because upstream names the `#include-once` cache files
//! after the MD5 of the URL. It is a filename, not a security decision: nothing
//! here should be used to authenticate anything.

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14,
    20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11,
    16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// Hex MD5 digest of `blob`, as `hashlib.md5(blob).hexdigest()` returns it.
pub fn md5_hex(blob: &[u8]) -> String {
    use std::fmt::Write as _;

    md5(blob).iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn md5(blob: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let mut tail = Vec::with_capacity(128);
    let rest = blob.len() % 64;
    tail.extend_from_slice(blob.get(blob.len() - rest..).unwrap_or_default());
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend_from_slice(&(blob.len() as u64).wrapping_mul(8).to_le_bytes());

    for chunk in blob.chunks_exact(64).chain(tail.chunks_exact(64)) {
        compress(&mut state, chunk);
    }

    let mut out = [0_u8; 16];
    for (slot, word) in out.chunks_exact_mut(4).zip(state) {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn compress(state: &mut [u32; 4], chunk: &[u8]) {
    let mut words = [0_u32; 16];
    for (slot, bytes) in words.iter_mut().zip(chunk.chunks_exact(4)) {
        *slot = u32::from_le_bytes(bytes.try_into().unwrap_or_default());
    }
    // `index` is always under 16, so the fallback below is unreachable.
    let word = |index: usize| words.get(index & 15).copied().unwrap_or(0);

    let [mut aa, mut bb, mut cc, mut dd] = *state;
    for (round, (shift, added)) in S.iter().zip(K.iter()).enumerate() {
        let (mix, index) = match round >> 4 {
            0 => ((bb & cc) | (!bb & dd), round),
            1 => ((dd & bb) | (!dd & cc), (5 * round + 1) % 16),
            2 => (bb ^ cc ^ dd, (3 * round + 5) % 16),
            _ => (cc ^ (bb | !dd), (7 * round) % 16),
        };
        let mix = mix
            .wrapping_add(aa)
            .wrapping_add(*added)
            .wrapping_add(word(index));
        aa = dd;
        dd = cc;
        cc = bb;
        bb = bb.wrapping_add(mix.rotate_left(*shift));
    }
    state[0] = state[0].wrapping_add(aa);
    state[1] = state[1].wrapping_add(bb);
    state[2] = state[2].wrapping_add(cc);
    state[3] = state[3].wrapping_add(dd);
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
    fn the_digests_match_the_rfc_1321_test_suite() {
        for (input, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890\
                 123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(md5_hex(input.as_bytes()), expected, "{input}");
        }
    }

    #[test]
    fn a_url_hashes_to_the_name_upstream_caches_it_under() {
        assert_eq!(
            md5_hex(b"http://example.com/seed"),
            "dd373076271182f4e96a73eb463f9acf"
        );
    }
}
