//! Parsing and validating a pushed manifest document.
//!
//! Manifest and descriptor types come from `oci-spec` rather than being
//! hand-rolled. What this module adds is the part `oci-spec` cannot: deciding
//! which of the two shapes a body is, projecting it onto the fields the key
//! schema stores, and turning every failure into `MANIFEST_INVALID` rather
//! than a serde message.

use std::collections::BTreeMap;

use oci_spec::image::{Descriptor, ImageIndex, ImageManifest, MediaType};
use serde::Deserialize;
use summ_core::{ChildRef, Digest, Platform};

use crate::error::{RegistryError, Result};

/// Media type stored when a body carries no `mediaType` and the push sent no
/// `Content-Type`. The spec's default for an unqualified manifest, and what the
/// conformance suite assumes.
pub const DEFAULT_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// One blob a manifest directly references - its config or one of its layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobDesc {
    pub digest: Digest,
    /// Declared size. Only trusted when the blob is not already known; a
    /// present `L` record is authoritative, because it was written from the
    /// bytes that actually arrived.
    pub size: u64,
    /// The descriptor carries `urls`, so the content lives somewhere else and
    /// this registry is not expected to hold it - a non-distributable, or
    /// "foreign", layer. Windows base layers are the case in the wild.
    ///
    /// Detected by the presence of `urls` rather than by media type: the
    /// `nondistributable` media types are the conventional carriers, but it is
    /// `urls` that says where the bytes actually are, and a registry that keyed
    /// off the media type would still reject a foreign layer wearing an
    /// ordinary one.
    pub foreign: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    Image,
    Index,
}

/// A manifest projected onto what the key schema stores.
#[derive(Debug, Clone)]
pub struct ParsedManifest {
    pub kind: ManifestKind,
    pub media_type: String,
    /// The manifest's own `artifactType`, verbatim.
    pub artifact_type: Option<String>,
    /// `artifactType` as a referrers response must report it, which is not the
    /// same field: for an image manifest it falls back to the config
    /// descriptor's media type, and for an index it does not fall back at all.
    pub referrer_artifact_type: Option<String>,
    pub annotations: BTreeMap<String, String>,
    pub subject: Option<Digest>,
    /// Config plus layers, in that order. Empty for an index, whose entries are
    /// manifests rather than blobs and so belong in `children`.
    pub blobs: Vec<BlobDesc>,
    pub children: Vec<ChildRef>,
}

impl ParsedManifest {
    /// Sum of the sizes of the blobs this manifest directly references.
    ///
    /// Includes the config and excludes foreign layers, so it agrees with the
    /// set of `R` edges the push writes and, more to the point, describes bytes
    /// this registry actually stores. Counting a Windows base layer hosted on
    /// someone else's CDN would inflate every repository size that mentions it.
    /// Not recursive: an index totals zero here, and its real size comes from
    /// walking its children and deduplicating shared layers.
    pub fn total_layer_size(&self) -> u64 {
        self.blobs
            .iter()
            .filter(|b| !b.foreign)
            .map(|b| b.size)
            .sum()
    }
}

/// Just enough of a manifest to decide its shape before committing to a type.
#[derive(Deserialize)]
struct Shape {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    manifests: Option<serde_json::Value>,
    config: Option<serde_json::Value>,
}

/// Parse a pushed manifest body.
///
/// `content_type` is the `Content-Type` of the push, parameters already
/// stripped. It is only a fallback: when the body carries a `mediaType`, that
/// wins. The alternative - store the header verbatim, as R1 first suggested -
/// loses whenever the two disagree, because the conformance suite derives the
/// media type it expects on a subsequent GET from the *body*. The spec makes
/// matching the pushed header a SHOULD and matching the body's `mediaType` a
/// MUST for the client, so following the body is both safer and better defined.
pub fn parse(body: &[u8], content_type: Option<&str>) -> Result<ParsedManifest> {
    let shape: Shape = serde_json::from_slice(body)
        .map_err(|e| RegistryError::invalid(format!("not a JSON object: {e}")))?;

    let media_type = shape
        .media_type
        .clone()
        .or_else(|| content_type.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_MEDIA_TYPE.to_string());

    match classify(&shape)? {
        ManifestKind::Index => parse_index(body, media_type),
        ManifestKind::Image => parse_image(body, media_type),
    }
}

/// Decide between the two shapes.
///
/// The `mediaType` field decides when it is one we know. When it is absent or
/// unrecognised - an artifact manifest with a custom type, say - fall back to
/// structure, because the two shapes are disjoint: an index has `manifests`, an
/// image manifest has `config`.
fn classify(shape: &Shape) -> Result<ManifestKind> {
    match shape.media_type.as_deref() {
        Some(OCI_INDEX) | Some(DOCKER_MANIFEST_LIST) => return Ok(ManifestKind::Index),
        Some(OCI_MANIFEST) | Some(DOCKER_MANIFEST) => return Ok(ManifestKind::Image),
        _ => {}
    }
    match (&shape.manifests, &shape.config) {
        (Some(_), None) => Ok(ManifestKind::Index),
        (None, Some(_)) => Ok(ManifestKind::Image),
        (Some(_), Some(_)) => Err(RegistryError::invalid(
            "has both 'manifests' and 'config'; neither an image manifest nor an index",
        )),
        (None, None) => Err(RegistryError::invalid(
            "has neither 'manifests' nor 'config'",
        )),
    }
}

fn parse_image(body: &[u8], media_type: String) -> Result<ParsedManifest> {
    let m: ImageManifest = serde_json::from_slice(body)
        .map_err(|e| RegistryError::invalid(format!("not an image manifest: {e}")))?;
    check_schema_version(m.schema_version())?;

    let artifact_type = m.artifact_type().as_ref().map(MediaType::to_string);
    // Referrers rule, from the spec via the conformance suite's expected
    // descriptors: an image manifest without an explicit artifactType reports
    // its config's media type instead.
    let referrer_artifact_type = artifact_type
        .clone()
        .or_else(|| Some(m.config().media_type().to_string()));

    let mut blobs = Vec::with_capacity(1 + m.layers().len());
    blobs.push(blob_desc(m.config())?);
    for layer in m.layers() {
        blobs.push(blob_desc(layer)?);
    }

    Ok(ParsedManifest {
        kind: ManifestKind::Image,
        media_type,
        artifact_type,
        referrer_artifact_type,
        annotations: annotations(m.annotations()),
        subject: m.subject().as_ref().map(descriptor_digest).transpose()?,
        blobs,
        children: Vec::new(),
    })
}

fn parse_index(body: &[u8], media_type: String) -> Result<ParsedManifest> {
    let ix: ImageIndex = serde_json::from_slice(body)
        .map_err(|e| RegistryError::invalid(format!("not an image index: {e}")))?;
    check_schema_version(ix.schema_version())?;

    let mut children = Vec::with_capacity(ix.manifests().len());
    for child in ix.manifests() {
        children.push(ChildRef {
            digest: descriptor_digest(child)?,
            platform: child.platform().as_ref().map(|p| Platform {
                os: p.os().to_string(),
                arch: p.architecture().to_string(),
                variant: p.variant().clone(),
            }),
        });
    }

    Ok(ParsedManifest {
        kind: ManifestKind::Index,
        media_type,
        artifact_type: ix.artifact_type().as_ref().map(MediaType::to_string),
        // An index with no artifactType omits the field entirely rather than
        // falling back to anything - it has no config to fall back to.
        referrer_artifact_type: ix.artifact_type().as_ref().map(MediaType::to_string),
        annotations: annotations(ix.annotations()),
        subject: ix.subject().as_ref().map(descriptor_digest).transpose()?,
        blobs: Vec::new(),
        children,
    })
}

fn check_schema_version(v: u32) -> Result<()> {
    if v == 2 {
        Ok(())
    } else {
        Err(RegistryError::invalid(format!(
            "schemaVersion must be 2, got {v}"
        )))
    }
}

fn blob_desc(d: &Descriptor) -> Result<BlobDesc> {
    Ok(BlobDesc {
        digest: descriptor_digest(d)?,
        size: d.size(),
        foreign: d.urls().as_ref().is_some_and(|u| !u.is_empty()),
    })
}

/// `oci-spec` validates a digest's *grammar*; this rejects the ones this
/// registry has no algorithm for, which is a `MANIFEST_INVALID` on push rather
/// than a `DIGEST_INVALID` on the reference.
fn descriptor_digest(d: &Descriptor) -> Result<Digest> {
    d.digest()
        .to_string()
        .parse()
        .map_err(|e| RegistryError::invalid(format!("descriptor digest: {e}")))
}

fn annotations(a: &Option<std::collections::HashMap<String, String>>) -> BTreeMap<String, String> {
    a.iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hexd(b: u8) -> String {
        format!("sha256:{}", format!("{b:02x}").repeat(32))
    }

    fn image_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.example.config.v1+json",
                        "digest": hexd(1), "size": 7 },
            "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                          "digest": hexd(2), "size": 100 } ],
            "annotations": { "org.opencontainers.image.title": "demo" }
        }))
        .expect("json")
    }

    #[test]
    fn a_layer_with_urls_is_foreign_and_does_not_count_towards_stored_size() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": hexd(1), "size": 7 },
            "layers": [
                { "mediaType": "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip",
                  "digest": hexd(2), "size": 123456,
                  "urls": ["https://store.example.com/blobs/sha256/aa"] },
                { "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                  "digest": hexd(3), "size": 100 }
            ]
        }))
        .expect("json");

        let p = parse(&body, None).expect("parses");
        assert_eq!(p.blobs.len(), 3, "a foreign layer is still referenced");
        assert!(!p.blobs[0].foreign, "the config is not foreign");
        assert!(p.blobs[1].foreign, "`urls` is what makes a layer foreign");
        assert!(!p.blobs[2].foreign);
        assert_eq!(
            p.total_layer_size(),
            107,
            "the 123456 bytes hosted elsewhere are not this registry's storage"
        );
    }

    #[test]
    fn an_empty_urls_array_is_not_a_foreign_layer() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": hexd(1), "size": 7 },
            "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                          "digest": hexd(2), "size": 100, "urls": [] } ]
        }))
        .expect("json");

        let p = parse(&body, None).expect("parses");
        assert!(
            !p.blobs[1].foreign,
            "an empty `urls` names nowhere to fetch from, so the blob is ours"
        );
        assert_eq!(p.total_layer_size(), 107);
    }

    #[test]
    fn an_image_manifest_projects_config_and_layers_onto_blobs() {
        let p = parse(&image_body(), None).unwrap();
        assert_eq!(p.kind, ManifestKind::Image);
        assert_eq!(p.blobs.len(), 2);
        assert_eq!(p.total_layer_size(), 107);
        assert_eq!(p.annotations.len(), 1);
    }

    #[test]
    fn an_image_manifest_without_artifact_type_reports_its_config_media_type() {
        let p = parse(&image_body(), None).unwrap();
        assert_eq!(p.artifact_type, None);
        assert_eq!(
            p.referrer_artifact_type.as_deref(),
            Some("application/vnd.example.config.v1+json")
        );
    }

    #[test]
    fn an_index_without_artifact_type_reports_none() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [ { "mediaType": OCI_MANIFEST, "digest": hexd(3), "size": 5,
                             "platform": { "os": "linux", "architecture": "arm64",
                                           "variant": "v8" } } ]
        }))
        .expect("json");
        let p = parse(&body, None).unwrap();
        assert_eq!(p.kind, ManifestKind::Index);
        assert_eq!(p.referrer_artifact_type, None);
        assert!(p.blobs.is_empty());
        let child = &p.children[0];
        let platform = child.platform.as_ref().expect("platform");
        assert_eq!(platform.os, "linux");
        assert_eq!(platform.arch, "arm64");
        assert_eq!(platform.variant.as_deref(), Some("v8"));
    }

    /// The record has to survive a postcard round trip, which it only does
    #[test]
    fn a_variant_less_platform_still_round_trips_through_postcard() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [ { "mediaType": OCI_MANIFEST, "digest": hexd(3), "size": 5,
                             "platform": { "os": "linux", "architecture": "amd64" } } ]
        }))
        .expect("json");
        let child = parse(&body, None).unwrap().children.remove(0);
        let bytes = postcard::to_allocvec(&child).expect("encode");
        let back: summ_core::ChildRef = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, child);
        assert_eq!(back.platform.expect("platform").variant, None);
    }

    #[test]
    fn shape_is_inferred_when_the_media_type_is_unknown() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.example.weird+json",
            "manifests": []
        }))
        .expect("json");
        assert_eq!(parse(&body, None).unwrap().kind, ManifestKind::Index);
    }

    #[test]
    fn the_body_media_type_beats_the_pushed_content_type() {
        let p = parse(&image_body(), Some(DOCKER_MANIFEST)).unwrap();
        assert_eq!(p.media_type, OCI_MANIFEST);
    }

    #[test]
    fn the_content_type_is_used_only_when_the_body_has_none() {
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": hexd(1), "size": 7 },
            "layers": []
        }))
        .expect("json");
        assert_eq!(
            parse(&body, Some(DOCKER_MANIFEST)).unwrap().media_type,
            DOCKER_MANIFEST
        );
        assert_eq!(parse(&body, None).unwrap().media_type, DEFAULT_MEDIA_TYPE);
    }

    #[test]
    fn malformed_bodies_are_manifest_invalid() {
        for body in [
            &b"not json"[..],
            b"{}",
            br#"{"schemaVersion":1,"config":{"mediaType":"a","digest":"sha256:00","size":1},"layers":[]}"#,
        ] {
            let err = parse(body, None).unwrap_err();
            assert_eq!(err.code(), crate::error::codes::MANIFEST_INVALID, "{err}");
        }
    }
}
