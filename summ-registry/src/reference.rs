//! `<reference>` - the tag-or-digest that addresses a manifest.

use std::fmt;
use std::str::FromStr;

use summ_core::Digest;

use crate::error::{RegistryError, Result};

/// Longest legal tag, from the spec's `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
const MAX_TAG_LEN: usize = 128;

/// A manifest reference: either a tag or a digest, and never anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

impl Reference {
    pub fn as_tag(&self) -> Option<&str> {
        match self {
            Reference::Tag(t) => Some(t),
            Reference::Digest(_) => None,
        }
    }

    pub fn as_digest(&self) -> Option<&Digest> {
        match self {
            Reference::Digest(d) => Some(d),
            Reference::Tag(_) => None,
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reference::Tag(t) => f.write_str(t),
            Reference::Digest(d) => write!(f, "{d}"),
        }
    }
}

impl FromStr for Reference {
    type Err = RegistryError;

    /// A reference containing `:` is being *offered* as a digest, so a parse
    /// failure is `DIGEST_INVALID` and not a tag.
    ///
    /// Falling back to "then it must be a tag" is the trap the conformance
    /// suite's `invalid-digest-format` case is built to catch: it would turn a
    /// 400 into a misleading 404. `:` is not in the tag grammar anyway, so the
    /// rule costs nothing.
    fn from_str(s: &str) -> Result<Self> {
        if s.contains(':') {
            return s.parse::<Digest>().map(Reference::Digest).map_err(|e| {
                RegistryError::DigestInvalid {
                    reason: e.to_string(),
                }
            });
        }
        validate_tag(s)?;
        Ok(Reference::Tag(s.to_string()))
    }
}

/// Check a tag against `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
///
/// Enforced inside this crate rather than at the edge because the `H` and
/// `A t` key ranges terminate a tag with NUL, which is only unambiguous while
/// the grammar guarantees a tag cannot contain one.
pub fn validate_tag(tag: &str) -> Result<()> {
    let invalid = |reason: &str| RegistryError::TagInvalid {
        tag: tag.to_string(),
        reason: reason.to_string(),
    };

    let mut chars = tag.chars();
    let Some(first) = chars.next() else {
        return Err(invalid("empty"));
    };
    if !(first.is_ascii_alphanumeric() || first == '_') {
        return Err(invalid("first character must be alphanumeric or '_'"));
    }
    if tag.chars().count() > MAX_TAG_LEN {
        return Err(invalid("longer than 128 characters"));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(invalid("characters must be [a-zA-Z0-9._-]"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_with_a_colon_is_a_digest_even_when_it_is_a_bad_one() {
        let err = "sha256:baddigeststring".parse::<Reference>().unwrap_err();
        assert_eq!(err.code(), crate::error::codes::DIGEST_INVALID);
    }

    #[test]
    fn tag_grammar_matches_the_spec() {
        for good in ["latest", "_v1", "v1.0.0-rc.1", "A", "0"] {
            assert!(good.parse::<Reference>().is_ok(), "{good}");
        }
        for bad in ["", ".hidden", "-lead", "has space", "has/slash"] {
            assert!(bad.parse::<Reference>().is_err(), "{bad}");
        }
        assert!("a".repeat(128).parse::<Reference>().is_ok());
        assert!("a".repeat(129).parse::<Reference>().is_err());
    }

    #[test]
    fn a_well_formed_digest_parses_as_one() {
        let s = "sha256:".to_string() + &"ab".repeat(32);
        assert!(matches!(
            s.parse::<Reference>().unwrap(),
            Reference::Digest(_)
        ));
    }
}
