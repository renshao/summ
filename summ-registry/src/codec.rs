//! Value encoding: postcard for records, zstd for manifest bodies.

use serde::de::DeserializeOwned;
use serde::Serialize;
use summ_core::SummError;

use crate::error::{RegistryError, Result};

/// zstd level for manifest bodies. Manifests are small, highly repetitive JSON
/// and are decompressed on every pull, so the default level is the right end of
/// the curve: a higher one costs push latency for a few hundred bytes.
const BODY_LEVEL: i32 = 3;

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    postcard::to_allocvec(value)
        .map_err(|e| RegistryError::Meta(SummError::InvalidData(format!("encode: {e}"))))
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    postcard::from_bytes(bytes).map_err(|_| RegistryError::corrupt(what))
}

/// Compress a manifest body for the `B` range.
///
/// The digest is over the bytes as pushed, so this must round-trip exactly -
/// no reserialisation of the parsed form, no whitespace normalisation. That is
/// also why the body is stored at all rather than rebuilt from
/// [`ManifestRecord`](summ_core::ManifestRecord): a re-encoded manifest would
/// have a different digest and every signature over it would break.
pub fn compress_body(body: &[u8]) -> Result<Vec<u8>> {
    zstd::stream::encode_all(body, BODY_LEVEL)
        .map_err(|e| RegistryError::Meta(SummError::Storage(format!("compress body: {e}"))))
}

pub fn decompress_body(stored: &[u8]) -> Result<Vec<u8>> {
    zstd::stream::decode_all(stored).map_err(|_| RegistryError::corrupt("manifest body"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_round_trips_byte_exact() {
        // Trailing newline and odd spacing included on purpose: the digest is
        // over exactly these bytes.
        let body = br#"{ "schemaVersion":2,  "layers": [] }
"#;
        assert_eq!(
            decompress_body(&compress_body(body).unwrap()).unwrap(),
            body
        );
    }
}
