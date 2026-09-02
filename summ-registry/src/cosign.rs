//! Legacy cosign artifact tags.
//!
//! Before OCI 1.1 gave manifests a `subject` field, cosign attached signatures
//! and attestations by *naming convention*: a signature over
//! `sha256:<hex>` is pushed as the tag `sha256-<hex>.sig`. That form still
//! dominates in the wild, and under this schema such an object is just another
//! tagged manifest with no `subject` and therefore no `F` edge.
//!
//! Leaving it that way is not a correctness bug - pull, push and the referrers
//! API are all spec-correct without it, and the referrers API is *supposed* to
//! omit these. It is a purge bug. Purge keys entirely off "is it tagged?", so
//! deleting the subject manifest leaves the signature tag pointing at a live
//! manifest forever, with its layers pinned by `R`, and nothing ever reclaims
//! it. zot reaps these explicitly (`gc.go:removeReferrer`); the cheaper fix
//! here is to synthesise the `F` edge at tag time so the ordinary path reaches
//! them.

use summ_core::Digest;

/// Suffixes cosign and friends use. `.att` is the attestation form; notation
/// and the sigstore bundle types use `subject` proper and need nothing here.
const ARTIFACT_SUFFIXES: [&str; 3] = ["sig", "sbom", "att"];

/// Recover the subject digest a legacy artifact tag names, if it is one.
///
/// The shape is `<algorithm>-<hex>.<suffix>`: the digest with its `:` replaced
/// by `-`, because `:` is not in the tag grammar. Anything that does not parse
/// as a digest this registry supports is an ordinary tag and returns `None` -
/// the check must never reject a tag, only recognise one.
pub fn subject_of_artifact_tag(tag: &str) -> Option<Digest> {
    let (head, suffix) = tag.rsplit_once('.')?;
    if !ARTIFACT_SUFFIXES.contains(&suffix) {
        return None;
    }
    let (algorithm, encoded) = head.split_once('-')?;
    format!("{algorithm}:{encoded}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: u8) -> String {
        format!("{b:02x}").repeat(32)
    }

    #[test]
    fn recognises_the_three_cosign_suffixes() {
        for suffix in ["sig", "sbom", "att"] {
            let tag = format!("sha256-{}.{suffix}", hex(0xab));
            let subject = subject_of_artifact_tag(&tag).expect(suffix);
            assert_eq!(subject, Digest::Sha256([0xab; 32]));
        }
    }

    #[test]
    fn sha512_artifact_tags_are_recognised_too() {
        let tag = format!("sha512-{}.sig", format!("{:02x}", 7).repeat(64));
        assert_eq!(subject_of_artifact_tag(&tag), Some(Digest::Sha512([7; 64])));
    }

    #[test]
    fn an_ordinary_tag_is_never_mistaken_for_an_artifact() {
        for tag in [
            "latest",
            "v1.0.0",
            "sha256-notlongenough.sig",
            // The referrers *fallback tag schema* has no suffix, so it stays an
            // ordinary tag - which is what the spec wants when the referrers
            // API has been available from the first push.
            &format!("sha256-{}", hex(1)),
            &format!("sha256-{}.tar", hex(1)),
        ] {
            assert_eq!(subject_of_artifact_tag(tag), None, "{tag}");
        }
    }
}
