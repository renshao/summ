//! Request handlers, one module per endpoint group.
//!
//! Method dispatch happens here rather than in the router because the router
//! matches a path *suffix* (see [`crate::app::route`]); by the time an
//! [`Endpoint`] exists the resource is known and the method is just a match arm.
//! Doing it this way also keeps `HEAD` a first-class path: axum's
//! `MethodRouter` would answer `HEAD` by running the `GET` handler and dropping
//! the body, and a `HEAD` that has to carry a real `Content-Length` with an
//! empty body is not that.

pub mod base;
pub mod blobs;
pub mod catalog;
pub mod manifests;
pub mod referrers;
pub mod tags;
pub mod uploads;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use summ_core::Digest;

use crate::app::{AppState, Endpoint};
use crate::config::ServerConfig;
use crate::error::{ApiError, ErrorCode};
use crate::query;
use crate::seam::{OpsError, Registry};

/// The digest of the bytes in the response, per spec §Pulling manifests and
/// §Pulling blobs.
///
/// summ emits this on every response that can carry one - manifest and blob
/// `GET`/`HEAD`, manifest `PUT`, blob `PUT`, mount - and always under the
/// algorithm the client used. The spec permits the value to differ from a
/// client-supplied digest when the algorithms differ; that escape hatch exists
/// for registries that rehash everything as sha256, which summ does not, so it
/// never applies here. Absence is tolerated by the conformance suite at
/// `OCI_VERSION=1.1` but a *wrong* value fails, and absence fails at `dev`.
pub const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");
/// Acknowledges that a pushed manifest's `subject` was processed.
pub const OCI_SUBJECT: HeaderName = HeaderName::from_static("oci-subject");
/// One per tag accepted from a `?tag=` parameter (end-7b).
pub const OCI_TAG: HeaderName = HeaderName::from_static("oci-tag");
/// Optional minimum chunk size advertised on an upload `POST`.
pub const OCI_CHUNK_MIN_LENGTH: HeaderName = HeaderName::from_static("oci-chunk-min-length");
/// Names the referrers filters that were actually applied.
pub const OCI_FILTERS_APPLIED: HeaderName = HeaderName::from_static("oci-filters-applied");

pub const MEDIA_TYPE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_TYPE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_TYPE_OCTET_STREAM: &str = "application/octet-stream";

/// Everything a handler needs about the request except its body.
pub struct Ctx {
    pub state: AppState,
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
}

impl Ctx {
    pub fn config(&self) -> &ServerConfig {
        &self.state.config
    }

    pub fn registry(&self) -> &dyn Registry {
        self.state.registry.as_ref()
    }

    pub fn header(&self, name: HeaderName) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        query::first(&self.query, key)
    }
}

pub async fn handle(state: AppState, endpoint: Endpoint, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let ctx = Ctx {
        state,
        method: parts.method,
        path: parts.uri.path().to_owned(),
        query: query::pairs(parts.uri.query().unwrap_or("")),
        headers: parts.headers,
    };

    let outcome = match endpoint {
        Endpoint::Base => base::handle(&ctx),
        Endpoint::Catalog => catalog::handle(&ctx).await,
        Endpoint::TagList { name } => tags::handle(&ctx, &name).await,
        Endpoint::Manifest { name, reference } => {
            manifests::handle(&ctx, &name, &reference, body).await
        }
        Endpoint::Blob { name, digest } => blobs::handle(&ctx, &name, &digest).await,
        Endpoint::Uploads { name } => uploads::create(&ctx, &name, body).await,
        Endpoint::Upload { name, id } => uploads::session(&ctx, &name, &id, body).await,
        Endpoint::Referrers { name, digest } => referrers::handle(&ctx, &name, &digest).await,
    };

    outcome.unwrap_or_else(IntoResponse::into_response)
}

pub type Handled = Result<Response, ApiError>;

/// `405` with the `Allow` header RFC 9110 requires on one.
pub fn method_not_allowed(allow: &'static str) -> ApiError {
    ApiError::new(ErrorCode::Unsupported)
        .with_message("method not allowed")
        .with_header(header::ALLOW, HeaderValue::from_static(allow))
}

/// Translate a failure from below into a spec error.
///
/// The one non-obvious mapping is [`OpsError::OffsetMismatch`]: an out-of-order
/// chunk is a `416`, and the code is `BLOB_UPLOAD_INVALID` rather than the
/// reference implementation's invented `RANGE_INVALID`, which is outside the
/// spec's set.
pub fn ops_error(err: OpsError) -> ApiError {
    match err {
        OpsError::RepoUnknown => ApiError::new(ErrorCode::NameUnknown),
        OpsError::ManifestUnknown => ApiError::new(ErrorCode::ManifestUnknown),
        OpsError::BlobUnknown => ApiError::new(ErrorCode::BlobUnknown),
        OpsError::UploadUnknown => ApiError::new(ErrorCode::BlobUploadUnknown),
        OpsError::OffsetMismatch { current } => ApiError::new(ErrorCode::BlobUploadInvalid)
            .with_status(StatusCode::RANGE_NOT_SATISFIABLE)
            .with_message("chunk is out of order")
            .with_detail(format!("expected the chunk to start at {current}")),
        OpsError::DigestMismatch => ApiError::new(ErrorCode::DigestInvalid),
        OpsError::ManifestInvalid(detail) => {
            ApiError::new(ErrorCode::ManifestInvalid).with_detail(detail)
        }
        OpsError::ManifestBlobUnknown { digest } => ApiError::new(ErrorCode::ManifestBlobUnknown)
            .with_detail(format!("{digest} is not present in this repository")),
        OpsError::SizeMismatch { declared, actual } => ApiError::new(ErrorCode::SizeInvalid)
            .with_detail(format!("expected {declared} bytes, got {actual}")),
        OpsError::BodyTooLarge { limit } => ApiError::new(ErrorCode::SizeInvalid)
            .with_status(StatusCode::PAYLOAD_TOO_LARGE)
            .with_message("request body exceeds the maximum accepted size")
            .with_detail(format!("limit is {limit} bytes")),
        OpsError::BodyIncomplete(detail) => ApiError::new(ErrorCode::BlobUploadInvalid)
            .with_message("the request body did not arrive complete")
            .with_detail(detail),
        OpsError::Internal(detail) => ApiError::internal(detail),
    }
}

/// Buffer a request body.
///
/// Blob bodies do **not** come through here any more - they travel as an
/// [`UploadBody`](crate::seam::UploadBody) and are written through frame by
/// frame. What is left is manifests, which must be hashed and stored whole
/// anyway and are capped at a few megabytes.
pub async fn read_body(body: Body, limit: usize, on_overflow: ApiError) -> Result<Bytes, ApiError> {
    axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| on_overflow)
}

/// Finish a response, turning the impossible builder error into a `500` rather
/// than a panic.
pub fn build(builder: axum::http::response::Builder, body: Body) -> Response {
    builder
        .body(body)
        .unwrap_or_else(|e| ApiError::internal(e.to_string()).into_response())
}

/// A response with an explicit `Content-Length` and no body.
///
/// Used for every `HEAD` and for the several `2XX`s the spec defines as
/// bodyless. The conformance suite asserts *both* halves on a `HEAD` - the
/// exact `Content-Length` and an empty body - so a framework that recomputes
/// `Content-Length` from the body it is about to send would fail. Setting it
/// here, explicitly, is why `HEAD` is dispatched as its own path.
pub fn empty_with_length(
    status: StatusCode,
    length: u64,
    headers: Vec<(HeaderName, HeaderValue)>,
) -> Response {
    let mut builder = axum::http::Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, length);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    build(builder, Body::empty())
}

/// `Docker-Content-Digest`'s value. Digest rendering is infallible ASCII, so
/// the fallback is unreachable rather than merely unlikely.
pub fn digest_header(digest: &Digest) -> HeaderValue {
    HeaderValue::from_str(&digest.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}

/// `Content-Type` with any parameters stripped.
///
/// The spec says a registry SHOULD ignore parameters on a pushed
/// `Content-Type` and SHOULD NOT include them on a served one. The suite
/// asserts the response type contains the manifest's own `mediaType` exactly,
/// so a stray `; charset=utf-8` on the way in must not survive to the way out.
pub fn media_type_of(raw: &str) -> &str {
    raw.split(';').next().unwrap_or(raw).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_parameters_are_stripped() {
        assert_eq!(media_type_of(MEDIA_TYPE_MANIFEST), MEDIA_TYPE_MANIFEST);
        assert_eq!(
            media_type_of("application/vnd.oci.image.index.v1+json; charset=utf-8"),
            MEDIA_TYPE_INDEX
        );
        assert_eq!(media_type_of("  text/plain  "), "text/plain");
    }
}
