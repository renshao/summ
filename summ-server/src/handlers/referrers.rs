//! end-12: `GET /v2/<name>/referrers/<digest>`.
//!
//! Disabled by default - PLAN.md defers the referrers API to Phase 6 - but
//! implemented, because the switch is [`ServerConfig::referrers_enabled`] and
//! the alternative would be discovering the endpoint's rules late.
//!
//! The rule that catches implementations out: **once the API is on it MUST NOT
//! return `404`.** An unknown subject digest, and even an unknown repository,
//! answer `200` with an empty `manifests` array. Only a malformed digest is a
//! `400`. That follows from the spec requiring a manifest with a dangling
//! `subject` to be accepted and then listed - referrers and their subject may
//! be pushed in either order, so "subject not found" is a normal state, not an
//! error.
//!
//! `artifactType` is resolved at push time, not here: for an image manifest it
//! falls back to the config descriptor's `mediaType`, and for an index it is
//! omitted entirely when absent. That difference between the two is easy to get
//! wrong and impossible to fix cheaply at read time.

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode};
use serde_json::{json, Map, Value};

use super::{
    build, method_not_allowed, ops_error, Ctx, Handled, MEDIA_TYPE_INDEX, OCI_FILTERS_APPLIED,
};
use crate::error::{ApiError, ErrorCode};
use crate::reference::parse_digest;
use crate::seam::{Descriptor, OpsError, Referrers};

pub async fn handle(ctx: &Ctx, name: &str, raw_digest: &str) -> Handled {
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        return Err(method_not_allowed("GET, HEAD"));
    }

    // Validated before the feature check, so a malformed digest is a `400`
    // whether or not the endpoint is switched on.
    let subject = parse_digest(raw_digest)
        .map_err(|e| ApiError::new(ErrorCode::DigestInvalid).with_detail(e.0))?;

    if !ctx.config().referrers_enabled {
        // Not implemented yet: `404`, matching a registry with no such route.
        // A client that gets this MUST fall back to the referrers tag schema,
        // which is exactly the behaviour we want until the `F` edges are being
        // served.
        return Err(ApiError::new(ErrorCode::Unsupported)
            .with_status(StatusCode::NOT_FOUND)
            .with_message("referrers API is not enabled"));
    }

    let artifact_type = ctx.param("artifactType").filter(|s| !s.is_empty());
    let referrers = match ctx
        .registry()
        .referrers(name, &subject, artifact_type)
        .await
    {
        Ok(referrers) => referrers,
        // An unknown repository must not become a `404` here either.
        Err(OpsError::RepoUnknown) => Referrers {
            manifests: Vec::new(),
            filter_applied: artifact_type.is_some(),
        },
        Err(err) => return Err(ops_error(err)),
    };

    // The suite checks `mediaType` and `schemaVersion` inside the body as well
    // as the `Content-Type` header, so all three are set.
    let body = json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_TYPE_INDEX,
        "manifests": referrers.manifests.iter().map(descriptor_json).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&body).map_err(|e| ApiError::internal(e.to_string()))?;

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, MEDIA_TYPE_INDEX)
        .header(header::CONTENT_LENGTH, bytes.len());
    if referrers.filter_applied {
        // Claimed only when the filter was exact: the suite then verifies no
        // descriptor of any other type is present. An unfiltered response to a
        // filtered query is legal - just do not claim the filter.
        builder = builder.header(
            OCI_FILTERS_APPLIED,
            HeaderValue::from_static("artifactType"),
        );
    }

    if ctx.method == Method::HEAD {
        return Ok(build(builder, Body::empty()));
    }
    Ok(build(builder, Body::from(bytes)))
}

fn descriptor_json(descriptor: &Descriptor) -> Value {
    let mut map = Map::new();
    map.insert("mediaType".into(), descriptor.media_type.clone().into());
    map.insert("digest".into(), descriptor.digest.to_string().into());
    map.insert("size".into(), descriptor.size.into());
    if let Some(artifact_type) = &descriptor.artifact_type {
        map.insert("artifactType".into(), artifact_type.clone().into());
    }
    if !descriptor.annotations.is_empty() {
        let annotations: Map<String, Value> = descriptor
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.clone())))
            .collect();
        map.insert("annotations".into(), Value::Object(annotations));
    }
    Value::Object(map)
}
