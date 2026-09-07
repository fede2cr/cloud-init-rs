//! The slice of Python's `uuid.UUID` that cloud-init actually uses: parsing a
//! textual UUID, re-rendering it canonically, and the little-endian view of the
//! first three fields that Azure's gen1 SMBIOS tables are written in.

/// A parsed UUID, held as the 16 bytes in RFC 4122 (big-endian) order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uuid([u8; 16]);

impl Uuid {
    /// `uuid.UUID(hex)`. Accepts a `urn:uuid:` prefix, surrounding braces and
    /// dashes anywhere, but not surrounding whitespace — matching `CPython`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix("urn:uuid:").unwrap_or(text);
        let text = text.strip_prefix('{').unwrap_or(text);
        let text = text.strip_suffix('}').unwrap_or(text);

        let mut bytes = [0u8; 16];
        let mut nibbles = text.chars().filter(|c| *c != '-');
        for byte in &mut bytes {
            let hi = nibbles.next()?.to_digit(16)?;
            let lo = nibbles.next()?.to_digit(16)?;
            *byte = u8::try_from(hi * 16 + lo).ok()?;
        }
        if nibbles.next().is_some() {
            return None;
        }
        Some(Self(bytes))
    }

    /// `uuid.uuid4()`: 122 random bits with the version and variant fields set.
    #[must_use]
    pub fn v4() -> Self {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&ci_sys::rand::u64().to_ne_bytes());
        bytes[8..].copy_from_slice(&ci_sys::rand::u64().to_ne_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(bytes)
    }

    /// `UUID(bytes=other.bytes_le)`: the first three fields are re-read with the
    /// opposite endianness. Applying it twice is the identity.
    #[must_use]
    pub fn byte_swapped(self) -> Self {
        let b = self.0;
        Self([
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11],
            b[12], b[13], b[14], b[15],
        ])
    }
}

impl std::fmt::Display for Uuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                write!(f, "-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::Uuid;

    #[test]
    fn the_shapes_python_accepts_all_parse_to_the_same_value() {
        let canonical = "12345678-1234-5678-1234-567812345678";
        for text in [
            canonical,
            "urn:uuid:12345678-1234-5678-1234-567812345678",
            "{12345678-1234-5678-1234-567812345678}",
            "12345678123456781234567812345678",
            "1234-5678-1234-5678-1234-5678-1234-5678",
            "ABCDEF78-1234-5678-1234-567812345678",
        ] {
            let parsed = Uuid::parse(text).unwrap();
            if text.starts_with("ABC") {
                assert_eq!(parsed.to_string(), "abcdef78-1234-5678-1234-567812345678");
            } else {
                assert_eq!(parsed.to_string(), canonical);
            }
        }
    }

    #[test]
    fn surrounding_whitespace_is_refused_the_way_python_refuses_it() {
        assert!(Uuid::parse("  12345678-1234-5678-1234-567812345678  ").is_none());
        assert!(Uuid::parse("12345678-1234-5678-1234-56781234567").is_none());
        assert!(Uuid::parse("12345678-1234-5678-1234-5678123456789").is_none());
        assert!(Uuid::parse("gggggggg-1234-5678-1234-567812345678").is_none());
        assert!(Uuid::parse("").is_none());
    }

    #[test]
    fn a_v4_uuid_carries_its_version_and_variant() {
        let text = Uuid::v4().to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(Uuid::v4(), Uuid::v4());
    }

    #[test]
    fn byte_swapping_reverses_the_first_three_fields_only() {
        let uuid = Uuid::parse("12345678-1234-5678-1234-567812345678").unwrap();
        assert_eq!(
            uuid.byte_swapped().to_string(),
            "78563412-3412-7856-1234-567812345678"
        );
        assert_eq!(uuid.byte_swapped().byte_swapped(), uuid);
    }
}
