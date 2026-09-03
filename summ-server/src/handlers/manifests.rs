//! end-3, end-7, end-9: `/v2/<name>/manifests/<tag-or-digest>`.
//!
//! Four decisions worth stating, because each has a plausible alternative that
//! is wrong.
//!
//! **`Accept` is not used to gate the response.** The reference implementation
//! answers `404 MANIFEST_UNKNOWN` for a stored OCI manifest when `Accept` does
//! not list its media type. That behaviour exists to protect schema1-only
//! Docker clients from being handed content they cannot parse - a problem summ
//! does not have, because it never converts or synthesises a manifest. Serving
//! the stored bytes regardless is strictly more useful, passes conformance (the
//! suite always sends a matching `Accept`), and removes a whole class of "works
//! with docker, 404s with curl" reports.
//!
//! **`Content-Type` is whatever was pushed, echoed verbatim, without
//! parameters.** The suite reads the stored manifest's own `mediaType` field
//! and asserts the response type matches it exactly.
//!
//! **`HEAD` is its own path**, two point lookups and no body read. It is the
//! first of the four serial metadata steps in a cold containerd pull, and
//! containerd falls back to a full `GET` it did not need if the `HEAD` lacks
//! `Docker-Content-Digest` or `Content-Length`.
//!
//! **A reference containing `:` is a digest even if it does not parse.** See
//! [`crate::reference::parse_reference`].

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode};

use super::{
    build, digest_header, empty_with_length, media_type_of, method_not_allowed, ops_error,
    read_body, Ctx, Handled, DOCKER_CONTENT_DIGEST, MEDIA_TYPE_MANIFEST, OCI_SUBJECT, OCI_TAG,
};
use crate::error::{ApiError, ErrorCode};
use crate::query;
use crate::reference::{parse_reference, valid_tag, Reference, ReferenceError};

pub async fn handle(ctx: &Ctx, name: &str, raw_reference: &str, body: Body) -> Handled {
    let writing = ctx.method == Method::PUT;
    let reference = parse_reference(raw_reference).map_err(|e| reference_error(e, writing))?;

    match ctx.method {
        Method::GET => get(ctx, name, &reference).await,
        Method::HEAD => head(ctx, name, &reference).await,
        Method::PUT => put(ctx, name, &reference, body).await,
        Method::DELETE => delete(ctx, name, &reference).await,
        _ => Err(method_not_allowed("GET, HEAD, PUT, DELETE")),
    }
}

/// A malformed digest is unambiguously a client error and the spec pins it to
/// `400`. A malformed *tag* is treated differently by method: on a write the
/// request is definitively invalid, but on a read an unrepresentable reference
/// simply cannot name anything, and `404` is the answer a client can act on.
/// The suite requires `400` on `PUT` and accepts `400` or `404` on `GET`.
fn reference_error(err: ReferenceError, writing: bool) -> ApiError {
    match err {
        ReferenceError::Digest(detail) => {
            ApiError::new(ErrorCode::DigestInvalid).with_detail(detail)
        }
        ReferenceError::Tag(detail) if writing => {
            ApiError::new(ErrorCode::ManifestInvalid).with_detail(detail)
        }
        ReferenceError::Tag(detail) => {
            ApiError::new(ErrorCode::ManifestUnknown).with_detail(detail)
        }
    }
}

async fn get(ctx: &Ctx, name: &str, reference: &Reference) -> Handled {
    let (stat, bytes) = ctx
        .registry()
        .get_manifest(name, reference)
        .await
        .map_err(ops_error)?;

    let etag = format!("\"{}\"", stat.digest);
    // Not in the spec, but the reference implementation ships it and the
    // referrers fallback tag schema explicitly points clients at conditional
    // requests as their only race protection. It costs nothing here: the digest
    // is already in hand.
    if if_none_match(ctx, &etag) {
        return Ok(empty_with_length(
            StatusCode::NOT_MODIFIED,
            0,
            vec![
                (DOCKER_CONTENT_DIGEST, digest_header(&stat.digest)),
                (header::ETAG, header_value(&etag)),
            ],
        ));
    }

    Ok(build(
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, stat.media_type)
            .header(header::CONTENT_LENGTH, bytes.len())
            .header(DOCKER_CONTENT_DIGEST, digest_header(&stat.digest))
            .header(header::ETAG, etag),
        Body::from(bytes),
    ))
}

async fn head(ctx: &Ctx, name: &str, reference: &Reference) -> Handled {
    let stat = ctx
        .registry()
        .stat_manifest(name, reference)
        .await
        .map_err(ops_error)?;

    Ok(empty_with_length(
        StatusCode::OK,
        stat.size,
        vec![
            (header::CONTENT_TYPE, header_value(&stat.media_type)),
            (DOCKER_CONTENT_DIGEST, digest_header(&stat.digest)),
            (header::ETAG, header_value(&format!("\"{}\"", stat.digest))),
        ],
    ))
}

async fn put(ctx: &Ctx, name: &str, reference: &Reference, body: Body) -> Handled {
    let limit = ctx.config().max_manifest_bytes;
    // Check the declared length first so an oversized push is refused before
    // the body is read, then again via the reader's limit for a chunked body
    // that declared nothing.
    if let Some(declared) = ctx
        .header(header::CONTENT_LENGTH)
        .and_then(|v| v.parse::<usize>().ok())
    {
        if declared > limit {
            return Err(too_large(limit));
        }
    }
    let bytes = read_body(body, limit, too_large(limit)).await?;

    let content_type = ctx
        .header(header::CONTENT_TYPE)
        .map(media_type_of)
        .filter(|s| !s.is_empty())
        .unwrap_or(MEDIA_TYPE_MANIFEST)
        .to_owned();

    let tags = tag_params(ctx)?;

    let result = ctx
        .registry()
        .put_manifest(name, reference, &content_type, &tags, bytes)
        .await
        .map_err(ops_error)?;

    let mut headers = vec![
        (
            header::LOCATION,
            header_value(&format!("/v2/{name}/manifests/{}", result.digest)),
        ),
        (DOCKER_CONTENT_DIGEST, digest_header(&result.digest)),
    ];
    // MUST be sent whenever a pushed manifest carries a `subject` and the
    // referrers API is implemented; it is how the client learns the registry
    // processed the subject rather than ignoring it. Conditioned on the
    // endpoint actually being served, because that is what the header claims:
    // sending it while `/referrers/` answers `404` tells a client the fallback
    // tag schema is unnecessary and, one request later, that it is mandatory.
    if let Some(subject) = result.subject {
        if ctx.config().referrers_enabled {
            headers.push((OCI_SUBJECT, digest_header(&subject)));
        }
    }
    // One header per accepted tag. The suite accepts either repeated headers or
    // one comma-separated header; repeated is the less ambiguous form.
    for tag in &result.tags {
        headers.push((OCI_TAG, header_value(tag)));
    }

    Ok(empty_with_length(StatusCode::CREATED, 0, headers))
}

async fn delete(ctx: &Ctx, name: &str, reference: &Reference) -> Handled {
    ctx.registry()
        .delete_manifest(name, reference)
        .await
        .map_err(ops_error)?;
    // `202 Accepted` is what the spec names, but the delete is already visible:
    // the suite issues a `HEAD` immediately afterwards and requires `404`, with
    // no retry and no grace period.
    Ok(empty_with_length(StatusCode::ACCEPTED, 0, Vec::new()))
}

/// `413`, with a message rather than an invented error code.
///
/// The spec asks for a maximum manifest size and a `413` above it but names no
/// code for the condition, so this reuses `MANIFEST_INVALID` - the manifest is
/// indeed not acceptable - rather than adding a fifteenth code.
fn too_large(limit: usize) -> ApiError {
    ApiError::new(ErrorCode::ManifestInvalid)
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .with_message("manifest exceeds the maximum accepted size")
        .with_detail(format!("limit is {limit} bytes"))
}

/// `?tag=` parameters (end-7b), validated against the tag grammar.
///
/// Optional in the spec, on by default at `OCI_VERSION=dev`, and cheap here -
/// each tag is one more key pair in the same batch.
fn tag_params(ctx: &Ctx) -> Result<Vec<String>, ApiError> {
    let raw = query::all(&ctx.query, "tag");
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.len() > ctx.config().max_tag_params {
        // The spec explicitly permits `414` when a registry's own limit on tag
        // parameters is exceeded.
        return Err(ApiError::new(ErrorCode::ManifestInvalid)
            .with_status(StatusCode::URI_TOO_LONG)
            .with_message("too many tag parameters")
            .with_detail(format!("limit is {}", ctx.config().max_tag_params)));
    }
    let mut tags = Vec::with_capacity(raw.len());
    for tag in raw {
        if !valid_tag(tag) {
            return Err(ApiError::new(ErrorCode::ManifestInvalid)
                .with_message("invalid tag parameter")
                .with_detail(tag.to_owned()));
        }
        tags.push(tag.to_owned());
    }
    Ok(tags)
}

/// `If-None-Match` matching, tolerating an unquoted value.
///
/// A `*` matches any existing representation, per RFC 9110.
fn if_none_match(ctx: &Ctx, etag: &str) -> bool {
    let Some(header) = ctx.header(header::IF_NONE_MATCH) else {
        return false;
    };
    header.split(',').map(str::trim).any(|candidate| {
        candidate == "*"
            || candidate == etag
            || candidate.trim_matches('"') == etag.trim_matches('"')
    })
}

/// Header values built from digests, media types and paths are all ASCII by
/// construction; the fallback exists so no path can panic on hostile input.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}
