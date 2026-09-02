//! Content digests.
//!
//! The OCI spec mandates `sha256` and permits `sha512`. Both are represented
//! inline (no heap allocation) and encoded into keys as a single algorithm
//! byte followed by the raw hash bytes, so a decoder can recover the length
//! from the first byte without a separate length prefix.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::SummError;

const ALGO_SHA256: u8 = 1;
const ALGO_SHA512: u8 = 2;

/// Ordering is derived: sha256 sorts before sha512, then by raw bytes. Raw-byte
/// order over a hash is the same order as its lowercase hex, so prefix scans
/// come back in the order the spec's pagination expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Digest {
    Sha256([u8; 32]),
    Sha512([u8; 64]),
}

/// Encoded key length for an algorithm byte, without needing a whole `Digest`.
///
/// The RocksDB prefix extractor classifies keys by reading the algorithm byte
/// out of a raw key, so it needs this without decoding. Exported as a function
/// rather than as the constants so it cannot drift from `encode_into`.
pub fn encoded_len_of(algo: u8) -> Option<usize> {
    match algo {
        ALGO_SHA256 => Some(33),
        ALGO_SHA512 => Some(65),
        _ => None,
    }
}

impl Digest {
    pub fn algorithm(&self) -> &'static str {
        match self {
            Digest::Sha256(_) => "sha256",
            Digest::Sha512(_) => "sha512",
        }
    }

    pub fn raw(&self) -> &[u8] {
        match self {
            Digest::Sha256(b) => b.as_slice(),
            Digest::Sha512(b) => b.as_slice(),
        }
    }

    fn algo_byte(&self) -> u8 {
        match self {
            Digest::Sha256(_) => ALGO_SHA256,
            Digest::Sha512(_) => ALGO_SHA512,
        }
    }

    /// Length this digest occupies inside a key.
    pub fn encoded_len(&self) -> usize {
        1 + self.raw().len()
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.algo_byte());
        out.extend_from_slice(self.raw());
    }

    /// Decode a digest from the head of `buf`, returning it and the number of
    /// bytes consumed.
    pub fn decode(buf: &[u8]) -> Option<(Digest, usize)> {
        match *buf.first()? {
            ALGO_SHA256 if buf.len() >= 33 => {
                let mut a = [0u8; 32];
                a.copy_from_slice(&buf[1..33]);
                Some((Digest::Sha256(a), 33))
            }
            ALGO_SHA512 if buf.len() >= 65 => {
                let mut a = [0u8; 64];
                a.copy_from_slice(&buf[1..65]);
                Some((Digest::Sha512(a), 65))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.algorithm())?;
        f.write_str(":")?;
        for b in self.raw() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Digest {
    type Err = SummError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (algo, hex) = s
            .split_once(':')
            .ok_or_else(|| SummError::InvalidDigest(format!("missing algorithm separator: {s}")))?;

        fn unhex<const N: usize>(hex: &str, s: &str) -> Result<[u8; N], SummError> {
            if hex.len() != N * 2 {
                return Err(SummError::InvalidDigest(format!(
                    "expected {} hex chars, got {}: {s}",
                    N * 2,
                    hex.len()
                )));
            }
            let mut out = [0u8; N];
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                    .map_err(|e| SummError::InvalidDigest(format!("invalid hex in {s}: {e}")))?;
            }
            Ok(out)
        }

        match algo {
            "sha256" => Ok(Digest::Sha256(unhex::<32>(hex, s)?)),
            "sha512" => Ok(Digest::Sha512(unhex::<64>(hex, s)?)),
            other => Err(SummError::InvalidDigest(format!(
                "unsupported algorithm {other:?}"
            ))),
        }
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if ser.is_human_readable() {
            ser.collect_str(self)
        } else {
            let mut buf = Vec::with_capacity(self.encoded_len());
            self.encode_into(&mut buf);
            serde_bytes_compat::serialize(&buf, ser)
        }
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        if de.is_human_readable() {
            let s = String::deserialize(de)?;
            s.parse().map_err(serde::de::Error::custom)
        } else {
            let buf: Vec<u8> = serde_bytes_compat::deserialize(de)?;
            Digest::decode(&buf)
                .map(|(d, _)| d)
                .ok_or_else(|| serde::de::Error::custom("malformed digest"))
        }
    }
}

mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        v.to_vec().serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        Vec::<u8>::deserialize(de)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_roundtrips_through_string() {
        let s = "sha256:".to_string() + &"ab".repeat(32);
        let d: Digest = s.parse().unwrap();
        assert_eq!(d.to_string(), s);
    }

    #[test]
    fn sha512_roundtrips_through_string() {
        let s = "sha512:".to_string() + &"3f".repeat(64);
        let d: Digest = s.parse().unwrap();
        assert_eq!(d.to_string(), s);
    }

    #[test]
    fn encoded_len_of_agrees_with_the_encoder() {
        for d in [Digest::Sha256([0; 32]), Digest::Sha512([0; 64])] {
            let mut buf = Vec::new();
            d.encode_into(&mut buf);
            assert_eq!(encoded_len_of(buf[0]), Some(d.encoded_len()));
        }
        assert_eq!(encoded_len_of(0), None);
        assert_eq!(encoded_len_of(99), None);
    }

    #[test]
    fn key_encoding_roundtrips_and_reports_length() {
        for d in [Digest::Sha256([7u8; 32]), Digest::Sha512([9u8; 64])] {
            let mut buf = Vec::new();
            d.encode_into(&mut buf);
            assert_eq!(buf.len(), d.encoded_len());
            let (back, used) = Digest::decode(&buf).unwrap();
            assert_eq!(back, d);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn rejects_unknown_algorithm_and_bad_length() {
        assert!("md5:abcd".parse::<Digest>().is_err());
        assert!("sha256:ab".parse::<Digest>().is_err());
        assert!("sha256".parse::<Digest>().is_err());
    }

    #[test]
    fn raw_byte_order_matches_hex_order() {
        let lo: Digest = ("sha256:".to_string() + &"00".repeat(32)).parse().unwrap();
        let hi: Digest = ("sha256:".to_string() + &"ff".repeat(32)).parse().unwrap();
        assert!(lo < hi);
        assert!(lo.to_string() < hi.to_string());
    }
}
