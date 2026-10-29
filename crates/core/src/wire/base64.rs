//! Standard base64 (RFC 4648 §4, padded), for PTY bytes on the wire. Decoding is strict: it rejects
//! anything `encode` would not have produced. A thin facade over the `base64` crate that maps its errors to ours.

use ::base64::Engine as _;
use ::base64::alphabet::STANDARD;
use ::base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// Padded on encode; on decode, padding must be canonical and trailing bits zero.
const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(true)
        .with_decode_allow_trailing_bits(false)
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
);

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
    ENGINE.encode(bytes)
}

pub fn decode(s: &str) -> Result<Vec<u8>, Base64Error> {
    use ::base64::DecodeError;
    if !s.len().is_multiple_of(4) {
        return Err(Base64Error::InvalidLength(s.len()));
    }
    ENGINE.decode(s).map_err(|e| match e {
        DecodeError::InvalidByte(_, b'=') => Base64Error::InvalidPadding,
        DecodeError::InvalidByte(at, byte) => Base64Error::InvalidByte { at, byte },
        DecodeError::InvalidLength(_) => Base64Error::InvalidLength(s.len()),
        // Non-zero trailing bits: two strings would decode to the same bytes.
        DecodeError::InvalidLastSymbol(..) | DecodeError::InvalidPadding => {
            Base64Error::InvalidPadding
        }
    })
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
