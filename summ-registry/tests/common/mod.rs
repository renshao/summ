// Each test binary compiles this module separately, so anything one of them
// does not use warns there.
#![allow(dead_code)]

//! Shared fixtures. Every test runs against a real `RedbEngine` on a temp
//! directory - the point of these tests is what actually lands in the store.

use std::sync::Arc;

use sha2::Digest as _;
use summ_core::Digest;
use summ_meta::RedbEngine;
use summ_registry::{ManifestPut, Reference, Registry, RegistryOptions};
use tempfile::TempDir;

pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

pub fn fixture() -> (TempDir, Registry) {
    fixture_with(RegistryOptions::default())
}

pub fn fixture_with(options: RegistryOptions) -> (TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let engine = RedbEngine::open(dir.path().join("meta.redb")).unwrap();
    (dir, Registry::with_options(Arc::new(engine), options))
}

pub fn sha256(bytes: &[u8]) -> Digest {
    let out = sha2::Sha256::digest(bytes);
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&out);
    Digest::Sha256(raw)
}

/// Pretend a blob upload completed: the bytes are notionally on disk, so `L`
/// and `P` go in.
pub fn upload(reg: &Registry, repo: &str, content: &str) -> (Digest, u64) {
    let bytes = content.as_bytes();
    let digest = sha256(bytes);
    reg.commit_blob(repo, &digest, bytes.len() as u64, 1_000)
        .unwrap();
    (digest, bytes.len() as u64)
}

pub fn descriptor(media_type: &str, digest: &Digest, size: u64) -> serde_json::Value {
    serde_json::json!({ "mediaType": media_type, "digest": digest.to_string(), "size": size })
}

/// An image manifest over one config and the given layers.
pub struct Image {
    pub config: (Digest, u64),
    pub layers: Vec<(Digest, u64)>,
    pub subject: Option<(Digest, u64)>,
    pub artifact_type: Option<String>,
    pub annotations: serde_json::Value,
}

impl Image {
    pub fn new(config: (Digest, u64)) -> Self {
        Self {
            config,
            layers: Vec::new(),
            subject: None,
            artifact_type: None,
            annotations: serde_json::json!({}),
        }
    }

    pub fn layer(mut self, layer: (Digest, u64)) -> Self {
        self.layers.push(layer);
        self
    }

    pub fn subject(mut self, subject: (Digest, u64)) -> Self {
        self.subject = Some(subject);
        self
    }

    pub fn artifact_type(mut self, t: &str) -> Self {
        self.artifact_type = Some(t.to_string());
        self
    }

    pub fn annotation(mut self, k: &str, v: &str) -> Self {
        self.annotations[k] = serde_json::Value::String(v.to_string());
        self
    }

    pub fn json(&self) -> Vec<u8> {
        let mut doc = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": OCI_MANIFEST,
            "config": descriptor(OCI_CONFIG, &self.config.0, self.config.1),
            "layers": self.layers.iter()
                .map(|(d, s)| descriptor(OCI_LAYER, d, *s))
                .collect::<Vec<_>>(),
            "annotations": self.annotations,
        });
        if let Some((d, s)) = &self.subject {
            doc["subject"] = descriptor(OCI_MANIFEST, d, *s);
        }
        if let Some(t) = &self.artifact_type {
            doc["artifactType"] = serde_json::Value::String(t.clone());
        }
        serde_json::to_vec(&doc).unwrap()
    }
}

/// An image index over the given children, each with a platform.
pub fn index_json(children: &[(Digest, u64, &str)]) -> Vec<u8> {
    let manifests: Vec<_> = children
        .iter()
        .map(|(d, s, arch)| {
            let mut desc = descriptor(OCI_MANIFEST, d, *s);
            desc["platform"] = serde_json::json!({ "os": "linux", "architecture": arch });
            desc
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX,
        "manifests": manifests,
    }))
    .unwrap()
}

pub fn put(reg: &Registry, repo: &str, reference: &str, body: &[u8], now: u64) -> Digest {
    push(reg, repo, reference, body, now).unwrap()
}

pub fn push(
    reg: &Registry,
    repo: &str,
    reference: &str,
    body: &[u8],
    now: u64,
) -> summ_registry::Result<Digest> {
    let reference: Reference = reference.parse()?;
    reg.put_manifest(&ManifestPut {
        repo,
        reference: &reference,
        body,
        content_type: Some(OCI_MANIFEST),
        now,
    })
    .map(|o| o.digest)
}
