//! Which hash an upload is being accumulated under.
//!
//! `summ_core::Digest` names an algorithm only once a hash *exists*. An upload
//! has to choose one at `POST` time, before a single byte has arrived, from the
//! spec's `?digest-algorithm=` parameter (end-4c). Hence a separate two-variant
//! enum rather than a sentinel `Digest`.
//!
//! `UploadSession.algorithm` stores the spec's name as a `String`, so
//! [`DigestAlgorithm::from_name`] and [`DigestAlgorithm::as_str`] are the
//! round trip through the metadata store.

use summ_core::{Result, SummError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DigestAlgorithm {
    Sha256,
    Sha512,
}

impl DigestAlgorithm {
    /// The spec's name, as it appears in a digest string and in
    /// `UploadSession.algorithm`.
    pub fn as_str(self) -> &'static str {
        match self {
            DigestAlgorithm::Sha256 => "sha256",
            DigestAlgorithm::Sha512 => "sha512",
        }
    }

    /// Parse the spec's name. An unknown algorithm is a client error the
    /// registry must reject with `400`, not something to fall back from.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "sha256" => Ok(DigestAlgorithm::Sha256),
            "sha512" => Ok(DigestAlgorithm::Sha512),
            other => Err(SummError::InvalidDigest(format!(
                "unsupported digest algorithm {other:?}"
            ))),
        }
    }

    /// The algorithm an existing digest was produced under, so a caller holding
    /// only a `?digest=` value can open the matching hasher.
    pub fn of(digest: &summ_core::Digest) -> Self {
        match digest {
            summ_core::Digest::Sha256(_) => DigestAlgorithm::Sha256,
            summ_core::Digest::Sha512(_) => DigestAlgorithm::Sha512,
        }
    }
}

impl std::fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
