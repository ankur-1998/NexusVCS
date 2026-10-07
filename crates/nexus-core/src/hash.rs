//! Object IDs: the SHA-256 of an object's canonical bytes (spec §4).

use std::fmt;

use sha2::{Digest as _, Sha256};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Length of the lowercase hex form.
    pub const HEX_LEN: usize = 64;
    /// Length of the abbreviated form shown by `nexus log --oneline`.
    pub const SHORT_LEN: usize = 12;

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses the 64-character lowercase hex form. Anything else is `None`.
    pub fn parse_hex(text: &str) -> Option<Self> {
        let text = text.as_bytes();
        if text.len() != Self::HEX_LEN {
            return None;
        }
        let mut bytes = [0; 32];
        let (pairs, _) = text.as_chunks::<2>();
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            *byte = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Some(Self(bytes))
    }

    /// The first [`Self::SHORT_LEN`] hex characters.
    pub fn short(&self) -> String {
        let mut text = self.to_string();
        text.truncate(Self::SHORT_LEN);
        text
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl ObjectId {
    /// The lowercase hex form as bytes, without allocating. Hashes are
    /// formatted constantly (object paths, tree text), so this avoids going
    /// through `fmt` 32 times per ID.
    pub fn to_hex(&self) -> [u8; 64] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = [0; 64];
        for (pair, byte) in hex.as_chunks_mut::<2>().0.iter_mut().zip(self.0) {
            pair[0] = DIGITS[usize::from(byte >> 4)];
            pair[1] = DIGITS[usize::from(byte & 0xf)];
        }
        hex
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        f.write_str(std::str::from_utf8(&hex).expect("hex digits are ASCII"))
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

/// Incremental SHA-256, for hashing an object's header and body without
/// first copying them into one buffer.
#[derive(Default)]
pub struct Hasher(Sha256);

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> ObjectId {
        ObjectId(self.0.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let mut hasher = Hasher::default();
        hasher.update(b"abc");
        let id = hasher.finish();
        // SHA-256("abc"), from FIPS 180-2.
        assert_eq!(
            id.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(ObjectId::parse_hex(&id.to_string()), Some(id));
        assert_eq!(id.short(), "ba7816bf8f01");
    }

    #[test]
    fn rejects_anything_but_full_lowercase_hex() {
        let upper = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        assert_eq!(ObjectId::parse_hex(upper), None);
        assert_eq!(ObjectId::parse_hex("ba7816bf"), None);
        assert_eq!(ObjectId::parse_hex(&"g".repeat(64)), None);
    }
}
