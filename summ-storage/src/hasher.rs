//! A resumable streaming hasher.
//!
//! Two properties matter, and both come from the same place.
//!
//! **The hasher advances as bytes are written.** A blob is never re-read to
//! verify it. zot's S3 path makes three full passes over every layer - complete
//! the multipart upload, re-read the whole object to hash it, then copy it to
//! the final key - which is the anti-pattern this exists to avoid.
//!
//! **The state serialises.** `sha2` 0.11 implements
//! `crypto_common::hazmat::SerializableState` for the buffered `Sha256` and
//! `Sha512` wrappers (40-byte core plus a 64-byte block buffer for sha256; 80
//! plus 128 for sha512). distribution does the same thing through Go's
//! `encoding.BinaryMarshaler`, but writes the state to files under
//! `hashstates/<algo>/<offset>` on the storage driver. A hundred-odd bytes fits
//! directly in the `UploadSession` record instead, which means an interrupted
//! chunked upload resumes on *any* process rather than only on the one holding
//! the files - the difference between a chunked upload being an HA constraint
//! and not being one.
//!
//! Note the trait does not exist in the `sha2` 0.10 line; this is why the
//! workspace pins 0.11.
//!
//! The serialised bytes are `hazmat` and are documented as potentially
//! sensitive. Nothing here exposes the hasher type itself - only
//! [`Hasher::serialize_state`] and [`Hasher::restore`], which is exactly the
//! handoff to `UploadSession.hasher_state` and back.

use sha2::digest::common::hazmat::{SerializableState, SerializedState};
use sha2::{Digest as _, Sha256, Sha512};
use summ_core::{Digest, Result, SummError};

use crate::algorithm::DigestAlgorithm;

#[derive(Clone)]
pub(crate) enum Hasher {
    Sha256(Box<Sha256>),
    Sha512(Box<Sha512>),
}

impl Hasher {
    pub(crate) fn new(algorithm: DigestAlgorithm) -> Self {
        match algorithm {
            DigestAlgorithm::Sha256 => Hasher::Sha256(Box::default()),
            DigestAlgorithm::Sha512 => Hasher::Sha512(Box::default()),
        }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(bytes),
            Hasher::Sha512(h) => h.update(bytes),
        }
    }

    /// Consume the hasher and produce the digest of everything fed to it.
    pub(crate) fn finalize(self) -> Digest {
        match self {
            Hasher::Sha256(h) => {
                let out = h.finalize();
                let mut raw = [0u8; 32];
                raw.copy_from_slice(&out);
                Digest::Sha256(raw)
            }
            Hasher::Sha512(h) => {
                let out = h.finalize();
                let mut raw = [0u8; 64];
                raw.copy_from_slice(&out);
                Digest::Sha512(raw)
            }
        }
    }

    /// Opaque state at the current offset, for `UploadSession.hasher_state`.
    pub(crate) fn serialize_state(&self) -> Vec<u8> {
        match self {
            Hasher::Sha256(h) => h.serialize().to_vec(),
            Hasher::Sha512(h) => h.serialize().to_vec(),
        }
    }

    /// Rebuild a hasher that has already absorbed the first `offset` bytes.
    ///
    /// A wrong-length or corrupt state is a hard error rather than a silent
    /// restart from zero: restarting would produce a digest over only the tail
    /// of the blob, which commit would then reject as a client digest mismatch
    /// - a correct outcome reported as the wrong party's fault.
    pub(crate) fn restore(algorithm: DigestAlgorithm, state: &[u8]) -> Result<Self> {
        fn bad(algorithm: DigestAlgorithm, want: usize, got: usize) -> SummError {
            SummError::Storage(format!(
                "corrupt {algorithm} hasher state: expected {want} bytes, got {got}"
            ))
        }
        fn undecodable(algorithm: DigestAlgorithm) -> SummError {
            SummError::Storage(format!(
                "corrupt {algorithm} hasher state: rejected by sha2"
            ))
        }

        match algorithm {
            DigestAlgorithm::Sha256 => {
                let arr: &SerializedState<Sha256> = state.try_into().map_err(|_| {
                    bad(algorithm, size_of::<SerializedState<Sha256>>(), state.len())
                })?;
                Sha256::deserialize(arr)
                    .map(|h| Hasher::Sha256(Box::new(h)))
                    .map_err(|_| undecodable(algorithm))
            }
            DigestAlgorithm::Sha512 => {
                let arr: &SerializedState<Sha512> = state.try_into().map_err(|_| {
                    bad(algorithm, size_of::<SerializedState<Sha512>>(), state.len())
                })?;
                Sha512::deserialize(arr)
                    .map(|h| Hasher::Sha512(Box::new(h)))
                    .map_err(|_| undecodable(algorithm))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialised_state_is_small_enough_to_live_in_an_upload_session() {
        // The point of storing this in `UploadSession` rather than on the
        // storage driver is that it is tiny. If it ever were not, the design
        // decision would need revisiting.
        assert_eq!(
            Hasher::new(DigestAlgorithm::Sha256).serialize_state().len(),
            104
        );
        assert_eq!(
            Hasher::new(DigestAlgorithm::Sha512).serialize_state().len(),
            208
        );
    }

    #[test]
    fn a_restored_hasher_matches_an_uninterrupted_one() {
        for algorithm in [DigestAlgorithm::Sha256, DigestAlgorithm::Sha512] {
            let mut straight = Hasher::new(algorithm);
            straight.update(b"the quick brown fox ");
            straight.update(b"jumps over the lazy dog");

            let mut interrupted = Hasher::new(algorithm);
            interrupted.update(b"the quick brown fox ");
            let state = interrupted.serialize_state();
            drop(interrupted);

            let mut resumed = Hasher::restore(algorithm, &state).expect("restore");
            resumed.update(b"jumps over the lazy dog");

            assert_eq!(straight.finalize(), resumed.finalize());
        }
    }

    #[test]
    fn a_truncated_state_is_rejected_rather_than_silently_restarted() {
        let state = Hasher::new(DigestAlgorithm::Sha256).serialize_state();
        assert!(Hasher::restore(DigestAlgorithm::Sha256, &state[..50]).is_err());
        // Right length, wrong algorithm: also a length mismatch, and caught.
        assert!(Hasher::restore(DigestAlgorithm::Sha512, &state).is_err());
    }
}
