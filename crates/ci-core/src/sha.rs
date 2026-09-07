//! SHA-1 and SHA-256, for `cc_ssh_authkey_fingerprints`.
//!
//! `authkey_hash` names a `hashlib` algorithm and the default is `sha256`, so
//! a fingerprint cannot be produced without one. They are written out here for
//! the same reason `hash::md5` is: adding a dependency to print a fingerprint
//! is a worse trade than eighty lines of well-specified arithmetic.
//!
//! Neither is used to authenticate anything. The fingerprint is printed on the
//! console for a human to compare by eye.

use std::fmt::Write as _;

/// `hashlib.new(name)` for the algorithms this port implements.
///
/// The name is lowercased first, as `hashlib`'s own constructor lookup does,
/// so `SHA256` and `sha256` are one algorithm.
///
/// `None` is upstream's `ValueError` for an unknown name, which
/// `_gen_fingerprint` turns into `"?"`.
#[must_use]
pub fn hexdigest(name: &str, blob: &[u8]) -> Option<String> {
    match name.to_lowercase().as_str() {
        "md5" => Some(super::hash::md5_hex(blob)),
        "sha1" => Some(hex(&sha1(blob))),
        "sha256" => Some(hex(&sha256(blob))),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The message with its `0x80` byte, zero padding and 64-bit big-endian bit
/// count, which both algorithms share.
fn padded(blob: &[u8]) -> Vec<u8> {
    let mut out = blob.to_vec();
    out.push(0x80);
    while out.len() % 64 != 56 {
        out.push(0);
    }
    let bits = (blob.len() as u64).wrapping_mul(8);
    out.extend_from_slice(&bits.to_be_bytes());
    out
}

fn word(chunk: &[u8], index: usize) -> u32 {
    let at = index * 4;
    u32::from_be_bytes([
        chunk.get(at).copied().unwrap_or_default(),
        chunk.get(at + 1).copied().unwrap_or_default(),
        chunk.get(at + 2).copied().unwrap_or_default(),
        chunk.get(at + 3).copied().unwrap_or_default(),
    ])
}

#[expect(
    clippy::many_single_char_names,
    reason = "a..h, k and w are the names the specification uses"
)]
fn sha1(blob: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    for chunk in padded(blob).chunks(64) {
        let mut w = [0u32; 80];
        for (index, slot) in w.iter_mut().take(16).enumerate() {
            *slot = word(chunk, index);
        }
        for index in 16..80 {
            let mixed = w.get(index - 3).copied().unwrap_or_default()
                ^ w.get(index - 8).copied().unwrap_or_default()
                ^ w.get(index - 14).copied().unwrap_or_default()
                ^ w.get(index - 16).copied().unwrap_or_default();
            if let Some(slot) = w.get_mut(index) {
                *slot = mixed.rotate_left(1);
            }
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in w.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (slot, add) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(add);
        }
    }
    let mut out = [0u8; 20];
    for (index, value) in h.iter().enumerate() {
        let bytes = value.to_be_bytes();
        for (offset, byte) in bytes.iter().enumerate() {
            if let Some(slot) = out.get_mut(index * 4 + offset) {
                *slot = *byte;
            }
        }
    }
    out
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[expect(
    clippy::many_single_char_names,
    reason = "a..h, k and w are the names the specification uses"
)]
fn sha256(blob: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    for chunk in padded(blob).chunks(64) {
        let mut w = [0u32; 64];
        for (index, slot) in w.iter_mut().take(16).enumerate() {
            *slot = word(chunk, index);
        }
        for index in 16..64 {
            let w15 = w.get(index - 15).copied().unwrap_or_default();
            let w2 = w.get(index - 2).copied().unwrap_or_default();
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            let value = w
                .get(index - 16)
                .copied()
                .unwrap_or_default()
                .wrapping_add(s0)
                .wrapping_add(w.get(index - 7).copied().unwrap_or_default())
                .wrapping_add(s1);
            if let Some(slot) = w.get_mut(index) {
                *slot = value;
            }
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (word, k) in w.iter().zip(SHA256_K) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(*word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(add);
        }
    }
    let mut out = [0u8; 32];
    for (index, value) in h.iter().enumerate() {
        let bytes = value.to_be_bytes();
        for (offset, byte) in bytes.iter().enumerate() {
            if let Some(slot) = out.get_mut(index * 4 + offset) {
                *slot = *byte;
            }
        }
    }
    out
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn the_published_test_vectors_agree() {
        assert_eq!(
            hexdigest("sha256", b"").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hexdigest("sha256", b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hexdigest("sha1", b"").unwrap(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hexdigest("sha1", b"abc").unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hexdigest("md5", b"abc").unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn a_message_that_straddles_the_padding_boundary_is_still_right() {
        // 55, 56 and 64 bytes: the last block with room for the length, the
        // one that needs a whole extra block, and an exact multiple.
        assert_eq!(
            hexdigest("sha256", &[b'a'; 55]).unwrap(),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            hexdigest("sha256", &[b'a'; 56]).unwrap(),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            hexdigest("sha256", &[b'a'; 64]).unwrap(),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn an_algorithm_this_port_does_not_have_is_the_value_error() {
        assert_eq!(hexdigest("sha512", b"abc"), None);
        assert_eq!(hexdigest("", b"abc"), None);
    }
}
