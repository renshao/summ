//! The three grammars a `/v2/` request is checked against: repository name,
//! tag, and digest (spec §Pulling manifests, and the OCI image-spec digest
//! grammar).
//!
//! These are written out by hand rather than pulled from a crate. There is no
//! maintained Rust crate that validates the *server side* `<name>` path
//! component - `oci_spec::distribution::Reference` parses client-side
//! `registry/repo:tag@digest` references, which is a different grammar - and
//! the whole of it is about forty lines.
//!
//! `summ_core::Digest` is the type these produce, but its `FromStr` is
//! deliberately permissive: it accepts uppercase hex because it exists to
//! decode values summ itself wrote. The spec's grammar is lowercase-only, and
//! the HTTP boundary is where that is enforced, so [`parse_digest`] checks the
//! character set before delegating.

use std::fmt;

use summ_core::Digest;

/// Repository names are capped at 255 bytes.
///
/// The spec gives no hard limit, only an implementers' note that clients cap
/// `host + "/" + <name>` at 255. `distribution/reference` turns that note into
/// `RepositoryNameTotalLengthMax = 255` on the path portion, and a name longer
/// than that is unusable by every real client, so rejecting it with a clear
/// `NAME_INVALID` beats storing something nobody can pull.
pub const MAX_NAME_LEN: usize = 255;

/// Tags are at most 128 characters: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
pub const MAX_TAG_LEN: usize = 128;

/// ```text
/// [a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*(\/[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*)*
/// ```
///
/// Lowercase only, path components separated by `/`, each component an
/// alphanumeric run followed by zero or more `separator alphanumeric-run`
/// pairs, where a separator is `.`, `_`, `__`, or one-or-more `-`.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    name.split('/').all(valid_name_component)
}

fn valid_name_component(component: &str) -> bool {
    let b = component.as_bytes();
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();

    let mut i = 0;
    loop {
        // An alphanumeric run. Required at the start of the component and after
        // every separator, which is what rejects `foo-`, `-foo` and `foo..bar`.
        let run_start = i;
        while i < b.len() && alnum(b[i]) {
            i += 1;
        }
        if i == run_start {
            return false;
        }
        if i == b.len() {
            return true;
        }

        match b[i] {
            b'.' => i += 1,
            // `_` or `__`, but not `___`.
            b'_' => {
                let start = i;
                while i < b.len() && b[i] == b'_' {
                    i += 1;
                }
                if i - start > 2 {
                    return false;
                }
            }
            // `-+`: any number of hyphens.
            b'-' => {
                while i < b.len() && b[i] == b'-' {
                    i += 1;
                }
            }
            _ => return false,
        }
    }
}

/// `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`. Mixed case is allowed for tags, unlike
/// for names.
pub fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    if b.is_empty() || b.len() > MAX_TAG_LEN {
        return false;
    }
    if !(b[0].is_ascii_alphanumeric() || b[0] == b'_') {
        return false;
    }
    b[1..]
        .iter()
        .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Parse a digest under the spec's grammar rather than summ-core's permissive
/// one.
///
/// The image-spec grammar is
/// `[a-z0-9]+(?:[.+_-][a-z0-9]+)*:[a-zA-Z0-9=_-]+` with per-algorithm encoded
/// forms that are lowercase hex. summ supports sha256 and sha512; a
/// well-formed digest naming any other algorithm is rejected here rather than
/// deeper down, because the spec asks for a `400` when the algorithm is
/// unsupported just as when the digest is malformed.
pub fn parse_digest(s: &str) -> Result<Digest, DigestError> {
    let Some((algorithm, encoded)) = s.split_once(':') else {
        return Err(DigestError(format!("missing algorithm separator: {s}")));
    };
    if algorithm.is_empty() || encoded.is_empty() {
        return Err(DigestError(format!("malformed digest: {s}")));
    }
    // Uppercase hex parses fine in `u8::from_str_radix` and would round-trip to
    // a *different* string, so a digest that differs only in case would be
    // silently accepted and then echoed back lowercased in
    // `Docker-Content-Digest`. The suite compares that header for exact
    // equality, so the case check has to happen before the parse.
    if !encoded
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(DigestError(format!(
            "digest encoding must be lowercase: {s}"
        )));
    }
    s.parse::<Digest>().map_err(|e| DigestError(format!("{e}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestError(pub String);

impl fmt::Display for DigestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `<tag-or-digest>` path component. The spec says it "MUST be either a tag
/// or a digest" and "MUST NOT be in any other format".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

impl Reference {
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

/// Why a `<tag-or-digest>` was rejected. The distinction matters: a malformed
/// digest is unambiguously a client error and the spec pins it to `400`,
/// whereas a malformed tag merely cannot name anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceError {
    Digest(String),
    Tag(String),
}

/// Disambiguate a `<tag-or-digest>`.
///
/// **The rule is the presence of `:`, not whether the digest parses.** A
/// reference containing a colon is being *offered* as a digest, so if it fails
/// digest parsing the answer is `DIGEST_INVALID`, not "fall back to treating it
/// as a tag and 404". The suite's `invalid-digest-format` case pushes to
/// `/v2/<name>/manifests/sha256:baddigeststring` and requires a `400` on PUT;
/// silently reinterpreting it as a tag would produce a misleading `404`. The
/// rule is self-consistent because `:` is not in the tag grammar either.
pub fn parse_reference(s: &str) -> Result<Reference, ReferenceError> {
    if s.contains(':') {
        parse_digest(s)
            .map(Reference::Digest)
            .map_err(|e| ReferenceError::Digest(e.0))
    } else if valid_tag(s) {
        Ok(Reference::Tag(s.to_owned()))
    } else {
        Err(ReferenceError::Tag(format!("invalid tag: {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_spec_name_examples() {
        for name in [
            "a",
            "foo",
            "foo.bar",
            "foo_bar",
            "foo__bar",
            "foo---bar",
            "a/b/c",
            "library/ubuntu",
            "homebrew/core/hello",
            "a1/b2.c3/d4__e5/f6---g7",
            "0",
        ] {
            assert!(valid_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_names_outside_the_grammar() {
        for name in [
            "",
            "Foo",       // uppercase
            "-foo",      // leading separator
            "foo-",      // trailing separator
            "foo..bar",  // `..` is not a separator
            "foo___bar", // `___` is not a separator
            "foo/",      // empty component
            "/foo",
            "foo//bar",
            "foo bar",
            "foo:bar",
            "foo@bar",
            "föö",
        ] {
            assert!(!valid_name(name), "{name} should be rejected");
        }
    }

    #[test]
    fn name_length_is_capped_at_255() {
        let ok = "a".repeat(MAX_NAME_LEN);
        assert!(valid_name(&ok));
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(!valid_name(&too_long));
    }

    #[test]
    fn accepts_and_rejects_tags_per_grammar() {
        for tag in [
            "v1",
            "V1",
            "_underscore",
            "1.0.0-alpha_1",
            "a".repeat(128).as_str(),
        ] {
            assert!(valid_tag(tag), "{tag} should be valid");
        }
        for tag in [
            "",
            ".leading-dot",
            "-leading-dash",
            "has/slash",
            "has:colon",
            "a".repeat(129).as_str(),
        ] {
            assert!(!valid_tag(tag), "{tag} should be rejected");
        }
    }

    #[test]
    fn digest_parsing_is_lowercase_only() {
        let lower = format!("sha256:{}", "ab".repeat(32));
        assert!(parse_digest(&lower).is_ok());

        let upper = format!("sha256:{}", "AB".repeat(32));
        assert!(
            parse_digest(&upper).is_err(),
            "uppercase hex must be rejected"
        );

        assert!(parse_digest("sha256:baddigeststring").is_err());
        assert!(parse_digest("sha256").is_err());
        assert!(parse_digest("md5:d41d8cd98f00b204e9800998ecf8427e").is_err());
        assert!(parse_digest(&format!("sha512:{}", "cd".repeat(64))).is_ok());
    }

    #[test]
    fn a_colon_bearing_reference_is_a_digest_even_when_malformed() {
        assert!(matches!(
            parse_reference("sha256:baddigeststring"),
            Err(ReferenceError::Digest(_))
        ));
        assert!(matches!(
            parse_reference("latest"),
            Ok(Reference::Tag(t)) if t == "latest"
        ));
        assert!(matches!(
            parse_reference("has/slash"),
            Err(ReferenceError::Tag(_))
        ));
    }
}
