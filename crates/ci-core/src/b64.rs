//! Standard base64, as `base64.b64encode`/`b64decode` produce it.

const ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[must_use]
pub fn encode(data: &[u8]) -> String {
    let symbol = |bits: u32, shift: u32| {
        let index = ((bits >> shift) & 0x3f) as usize;
        char::from(ALPHABET.get(index).copied().unwrap_or(b'A'))
    };
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let (b0, b1, b2) = (
            chunk.first().map_or(0, |b| u32::from(*b)),
            chunk.get(1).map_or(0, |b| u32::from(*b)),
            chunk.get(2).map_or(0, |b| u32::from(*b)),
        );
        let bits = (b0 << 16) | (b1 << 8) | b2;
        out.push(symbol(bits, 18));
        out.push(symbol(bits, 12));
        out.push(if chunk.len() > 1 {
            symbol(bits, 6)
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            symbol(bits, 0)
        } else {
            '='
        });
    }
    out
}

/// `None` for anything that is not well-formed base64. Whitespace is skipped,
/// which is what `b64decode` does for the MIME variant and what a value split
/// across lines in a YAML document needs.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len().div_ceil(4) * 3);
    let mut bits: u32 = 0;
    let mut have = 0;
    for byte in text.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let value = u32::try_from(ALPHABET.iter().position(|c| *c == byte)?).ok()?;
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            #[allow(clippy::cast_possible_truncation)]
            out.push((bits >> have) as u8);
        }
    }
    // Leftover bits must be padding, i.e. zero.
    if bits & ((1 << have) - 1) != 0 {
        return None;
    }
    Some(out)
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
    fn every_padding_length_round_trips() {
        for len in 0..32_u8 {
            let data: Vec<u8> = (0..len).collect();
            let text = encode(&data);
            assert_eq!(text.len() % 4, 0, "{text}");
            assert_eq!(decode(&text).unwrap(), data, "{text}");
        }
    }

    #[test]
    fn the_encoding_matches_the_one_python_prints() {
        assert_eq!(encode(b"hello-world"), "aGVsbG8td29ybGQ=");
        assert_eq!(encode(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(encode(b"#cloud-config\n"), "I2Nsb3VkLWNvbmZpZwo=");
    }

    #[test]
    fn a_character_outside_the_alphabet_is_not_base64() {
        assert!(decode("aGVsbG8*").is_none());
    }

    #[test]
    fn whitespace_between_groups_is_ignored() {
        assert_eq!(decode("aGVs\nbG8=\n").unwrap(), b"hello");
    }
}
