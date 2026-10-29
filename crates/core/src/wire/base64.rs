//! Standard base64 (RFC 4648 §4, padded), for PTY bytes on the wire. Decoding is strict: it rejects
//! anything `encode` would not have produced.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why a string is not canonical padded base64.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Base64Error {
    #[error("base64 length {0} is not a multiple of 4")]
    InvalidLength(usize),
    #[error("invalid base64 byte {byte:#04x} at offset {at}")]
    InvalidByte { at: usize, byte: u8 },
    #[error("misplaced base64 padding")]
    InvalidPadding,
}

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn decode(s: &str) -> Result<Vec<u8>, Base64Error> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return Err(Base64Error::InvalidLength(s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let quads = s.len() / 4;
    for (q, quad) in s.chunks(4).enumerate() {
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && q + 1 != quads) {
            return Err(Base64Error::InvalidPadding);
        }
        let mut n = 0u32;
        for (i, &c) in quad[..4 - pad].iter().enumerate() {
            let v = sextet(c).ok_or(if c == b'=' {
                Base64Error::InvalidPadding
            } else {
                Base64Error::InvalidByte {
                    at: q * 4 + i,
                    byte: c,
                }
            })?;
            n |= u32::from(v) << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        let keep = 3 - pad;
        // Bits past the last kept byte must be zero, or two strings would decode to the same bytes.
        if bytes[1 + keep..].iter().any(|&b| b != 0) {
            return Err(Base64Error::InvalidPadding);
        }
        out.extend_from_slice(&bytes[1..1 + keep]);
    }
    Ok(out)
}

fn sextet(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `#[serde(with = "base64::serde_bytes")]` for a `Vec<u8>` carried as a base64 string.
pub(crate) mod serde_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 §10 test vectors.
    const VECTORS: &[(&str, &str)] = &[
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn encode_matches_rfc4648_vectors() {
        for (plain, encoded) in VECTORS {
            assert_eq!(encode(plain.as_bytes()), *encoded, "encoding {plain:?}");
        }
    }

    #[test]
    fn decode_matches_rfc4648_vectors() {
        for (plain, encoded) in VECTORS {
            assert_eq!(
                decode(encoded).unwrap(),
                plain.as_bytes(),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn every_byte_value_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..all.len() {
            assert_eq!(decode(&encode(&all[..len])).unwrap(), &all[..len]);
        }
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn decode_rejects_unpadded_length() {
        assert_eq!(decode("Zm8"), Err(Base64Error::InvalidLength(3)));
    }

    #[test]
    fn decode_rejects_bytes_outside_the_alphabet() {
        assert_eq!(
            decode("Zm9v!mFy"),
            Err(Base64Error::InvalidByte { at: 4, byte: b'!' })
        );
        assert_eq!(
            decode("Zm9-"),
            Err(Base64Error::InvalidByte { at: 3, byte: b'-' })
        );
    }

    #[test]
    fn decode_rejects_misplaced_padding() {
        assert_eq!(decode("Zg==Zm9v"), Err(Base64Error::InvalidPadding));
        assert_eq!(decode("Z==="), Err(Base64Error::InvalidPadding));
        assert_eq!(decode("Z=g="), Err(Base64Error::InvalidPadding));
    }

    #[test]
    fn decode_rejects_nonzero_trailing_bits() {
        assert_eq!(decode("Zh=="), Err(Base64Error::InvalidPadding));
    }
}
