//! Failures that carry enough structure for the HTTP layer to pick a spec code.
//!
//! The Distribution Spec closes its error set to fourteen codes, and the
//! conformance suite checks the code, not the message. A stringly-typed error
//! would force the handler to guess, so every failure this crate can produce
//! names its own condition and maps to exactly one code through [`code`].
//!
//! [`code`]: RegistryError::code

use std::fmt;

use summ_core::{Digest, SummError};

/// Spec error codes, from `distribution-spec` §Error Codes. Only the ones this
/// crate can raise; the upload and auth codes belong to layers above it.
pub mod codes {
    pub const BLOB_UNKNOWN: &str = "BLOB_UNKNOWN";
    pub const DIGEST_INVALID: &str = "DIGEST_INVALID";
    pub const MANIFEST_BLOB_UNKNOWN: &str = "MANIFEST_BLOB_UNKNOWN";
    pub const MANIFEST_INVALID: &str = "MANIFEST_INVALID";
    pub const MANIFEST_UNKNOWN: &str = "MANIFEST_UNKNOWN";
    pub const NAME_INVALID: &str = "NAME_INVALID";
    pub const NAME_UNKNOWN: &str = "NAME_UNKNOWN";
    /// Outside the fourteen. Reserved for a genuine internal fault, which is a
    /// 500 and not a client's problem.
    pub const UNKNOWN: &str = "UNKNOWN";
}

#[derive(Debug)]
pub enum RegistryError {
    /// The repository has never been written to. `NAME_UNKNOWN`, 404.
    NameUnknown { repo: String },

    /// A tag that does not match the spec's tag grammar. `NAME_INVALID`, 400.
    ///
    /// This is not merely cosmetic: the `H` and `A t` key ranges use NUL as the
    /// terminator after a tag, an encoding that is only unambiguous because the
    /// grammar excludes NUL. Writing an ungrammatical tag would corrupt those
    /// scans, so the check is enforced here rather than left to the handler.
    TagInvalid { tag: String, reason: String },

    /// The body is not a manifest this registry can store. `MANIFEST_INVALID`,
    /// 400.
    ManifestInvalid { reason: String },

    /// No such manifest, or no such tag. `MANIFEST_UNKNOWN`, 404.
    ManifestUnknown { repo: String, reference: String },

    /// A manifest referenced a blob or a child manifest that is not present in
    /// this repository. `MANIFEST_BLOB_UNKNOWN`, 400.
    ///
    /// Deliberately *not* raised for a dangling `subject`: the spec requires a
    /// registry to accept a manifest whose subject does not exist yet, so that
    /// a client may push a referrer and its subject in either order.
    ManifestBlobUnknown { repo: String, digest: Digest },

    /// A reference that offered itself as a digest but did not parse, or a
    /// digest that did not match the bytes pushed. `DIGEST_INVALID`, 400.
    DigestInvalid { reason: String },

    /// The blob is not known to this repository. `BLOB_UNKNOWN`, 404.
    BlobUnknown { repo: String, digest: Digest },

    /// The metadata store failed, or a stored record would not decode. Not a
    /// client error.
    Meta(SummError),
}

impl RegistryError {
    /// The spec error code for this failure. The handler pairs it with a status
    /// from the spec's table; the mapping is 1:1, so the code is the whole
    /// decision.
    pub fn code(&self) -> &'static str {
        match self {
            RegistryError::NameUnknown { .. } => codes::NAME_UNKNOWN,
            RegistryError::TagInvalid { .. } => codes::NAME_INVALID,
            RegistryError::ManifestInvalid { .. } => codes::MANIFEST_INVALID,
            RegistryError::ManifestUnknown { .. } => codes::MANIFEST_UNKNOWN,
            RegistryError::ManifestBlobUnknown { .. } => codes::MANIFEST_BLOB_UNKNOWN,
            RegistryError::DigestInvalid { .. } => codes::DIGEST_INVALID,
            RegistryError::BlobUnknown { .. } => codes::BLOB_UNKNOWN,
            RegistryError::Meta(_) => codes::UNKNOWN,
        }
    }

    /// Whether the caller did something wrong. `false` means the store did, and
    /// the response is a 500 rather than anything in the 4xx range.
    pub fn is_client_error(&self) -> bool {
        !matches!(self, RegistryError::Meta(_))
    }

    pub(crate) fn invalid(reason: impl Into<String>) -> Self {
        RegistryError::ManifestInvalid {
            reason: reason.into(),
        }
    }

    pub(crate) fn corrupt(what: &str) -> Self {
        RegistryError::Meta(SummError::InvalidData(format!(
            "malformed stored record: {what}"
        )))
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::NameUnknown { repo } => {
                write!(f, "repository name not known to registry: {repo}")
            }
            RegistryError::TagInvalid { tag, reason } => {
                write!(f, "invalid tag {tag:?}: {reason}")
            }
            RegistryError::ManifestInvalid { reason } => write!(f, "manifest invalid: {reason}"),
            RegistryError::ManifestUnknown { repo, reference } => {
                write!(f, "manifest unknown: {repo}@{reference}")
            }
            RegistryError::ManifestBlobUnknown { repo, digest } => {
                write!(f, "{digest} is not present in {repo}")
            }
            RegistryError::DigestInvalid { reason } => write!(f, "invalid digest: {reason}"),
            RegistryError::BlobUnknown { repo, digest } => {
                write!(f, "blob unknown to registry: {digest} in {repo}")
            }
            RegistryError::Meta(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::Meta(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SummError> for RegistryError {
    fn from(e: SummError) -> Self {
        RegistryError::Meta(e)
    }
}

pub type Result<T> = std::result::Result<T, RegistryError>;
