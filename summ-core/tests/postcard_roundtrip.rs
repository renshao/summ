//! postcard is not self-describing: a skipped field leaves the decoder reading
//! the next field's bytes. `skip_serializing_if` is therefore never safe on a
//! record that is stored, and this test is what stops it coming back.

use summ_core::digest::Digest;
use summ_core::types::{ChildRef, Platform};

#[test]
fn a_platform_with_no_variant_roundtrips() {
    let p = Platform {
        os: "linux".into(),
        arch: "amd64".into(),
        variant: None,
    };
    let bytes = postcard::to_allocvec(&p).expect("encode");
    let back: Platform = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(back, p);
}

#[test]
fn an_enclosing_record_still_decodes_when_the_variant_is_absent() {
    let c = ChildRef {
        digest: Digest::Sha256([1u8; 32]),
        platform: Some(Platform {
            os: "linux".into(),
            arch: "amd64".into(),
            variant: None,
        }),
    };
    let bytes = postcard::to_allocvec(&c).expect("encode");
    let back: ChildRef = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(back, c);
}

/// Every stored record, encoded and decoded. The point is not that serde works;
/// it is that no future attribute quietly makes one of these unreadable, the way
/// `skip_serializing_if` did.
#[test]
fn every_stored_record_roundtrips() {
    use std::collections::BTreeMap;
    use summ_core::types::*;

    macro_rules! check {
        ($v:expr) => {{
            let v = $v;
            let bytes = postcard::to_allocvec(&v).expect("encode");
            assert_eq!(postcard::from_bytes(&bytes).ok(), Some(v), "roundtrip");
        }};
    }

    let d = Digest::Sha256([2u8; 32]);
    let mut annotations = BTreeMap::new();
    annotations.insert(
        "org.opencontainers.image.title".to_string(),
        "x".to_string(),
    );

    check!(ManifestRecord {
        repo: 1,
        digest: d,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        size: 512,
        total_layer_size: 4096,
        platform: Some(Platform {
            os: "linux".into(),
            arch: "amd64".into(),
            variant: None
        }),
        layers: vec![d],
        children: vec![ChildRef {
            digest: d,
            platform: None
        }],
        subject: Some(d),
        artifact_type: None,
        annotations: annotations.clone(),
        pushed_at: 1_700_000_000,
    });
    check!(BlobRecord { size: 1 });
    check!(RepoBlobRecord {
        size: 1,
        added_at: 2
    });
    check!(TagRecord {
        digest: d,
        tagged_at: 3
    });
    check!(ReferrerRecord {
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: None,
        size: 7,
        annotations,
    });
    check!(UploadSession {
        repo: 1,
        offset: 0,
        started_at: 1,
        updated_at: 2,
        algorithm: "sha256".into(),
        hasher_state: None,
    });
    check!(TagEvent {
        event: TagEventKind::Created,
        media_type: "m".into(),
        size: 9
    });
    check!(CounterBucket {
        manifest_pulls: 1,
        blob_pulls: 2,
        bytes_out: 3
    });
    check!(ManifestRef { repo: 4, digest: d });
}
