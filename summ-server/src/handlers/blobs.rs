//! end-2, end-10: `/v2/<name>/blobs/<digest>`.
//!
//! `Range` support is a spec SHOULD and a conformance MUST: the suite tests six
//! cases against a 2048-byte blob and records a fallback to a full `200` as a
//! *failure* of the `Blob get range` row. It is also what makes containerd's
//! resume and chunked-fetch paths work at all - without it containerd logs
//! "remote host ignored content range" and collapses to a single stream.
//!
//! Nothing on this path transforms the body. The digest is over the plaintext
//! bytes, so a `Content-Encoding` or a compression middleware here would make
//! every pull fail its digest check; containerd advertises
//! `zstd;q=1.0, gzip;q=0.8` and *will* transparently decode whatever the
//! response claims.

use axum::http::{header, HeaderValue, Method, StatusCode};

use super::{
    build, digest_header, empty_with_length, method_not_allowed, ops_error, Ctx, Handled,
    DOCKER_CONTENT_DIGEST, MEDIA_TYPE_OCTET_STREAM,
};
use crate::error::{ApiError, ErrorCode};
use crate::range::{parse_range, RangeOutcome};
use crate::reference::parse_digest;

pub async fn handle(ctx: &Ctx, name: &str, raw_digest: &str) -> Handled {
    let digest = parse_digest(raw_digest)
        .map_err(|e| ApiError::new(ErrorCode::DigestInvalid).with_detail(e.0))?;

    match ctx.method {
        Method::GET => get(ctx, name, &digest).await,
        Method::HEAD => head(ctx, name, &digest).await,
        Method::DELETE => delete(ctx, name, &digest).await,
        _ => Err(method_not_allowed("GET, HEAD, DELETE")),
    }
}

async fn get(ctx: &Ctx, name: &str, digest: &summ_core::Digest) -> Handled {
    // The size is needed to resolve the range before any bytes are read, and it
    // is a point lookup either way.
    let total = ctx
        .registry()
        .stat_blob(name, digest)
        .await
        .map_err(ops_error)?;

    let outcome = match ctx.header(header::RANGE) {
        Some(raw) => parse_range(raw, total),
        None => RangeOutcome::Whole,
    };

    let window = match outcome {
        RangeOutcome::Whole => None,
        RangeOutcome::Partial(range) => Some(range),
        // RFC 9110 §15.5.17: the `Content-Range` on a `416` names the selected
        // representation's length with `*` as the range. No body: none of the
        // spec's fourteen error codes describes an unsatisfiable range, and
        // inventing one (the reference implementation's `RANGE_INVALID`) buys
        // nothing a client can use.
        RangeOutcome::Unsatisfiable => {
            return Err(
                ApiError::status_only(StatusCode::RANGE_NOT_SATISFIABLE).with_header(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{total}"))
                        .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
                ),
            );
        }
    };

    let read = ctx
        .registry()
        .get_blob(name, digest, window)
        .await
        .map_err(ops_error)?;

    let mut builder = axum::http::Response::builder()
        .header(header::CONTENT_TYPE, MEDIA_TYPE_OCTET_STREAM)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(DOCKER_CONTENT_DIGEST, digest_header(digest));

    builder = match read.window {
        Some(range) => builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_LENGTH, range.size())
            .header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", range.start, range.end, read.total_size),
            ),
        None => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, read.total_size),
    };

    // Metered rather than counted from `Content-Length`: containerd 2.1+ asks
    // for `bytes=N-`, reads 8 MiB and tears the connection down, so counting
    // the window it asked for would over-report a 900 MB layer about a hundred
    // times. The wrapper reports what reached the socket, on drop.
    let body = ctx.counters().meter_blob(name, read.body);
    Ok(build(builder, body))
}

async fn head(ctx: &Ctx, name: &str, digest: &summ_core::Digest) -> Handled {
    let size = ctx
        .registry()
        .stat_blob(name, digest)
        .await
        .map_err(ops_error)?;

    // `Range` is deliberately ignored on `HEAD`: there is no body to partition,
    // and answering `206` would only invite a client to compute an offset from
    // a response that carries no bytes.
    Ok(empty_with_length(
        StatusCode::OK,
        size,
        vec![
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(MEDIA_TYPE_OCTET_STREAM),
            ),
            (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
            (DOCKER_CONTENT_DIGEST, digest_header(digest)),
        ],
    ))
}

async fn delete(ctx: &Ctx, name: &str, digest: &summ_core::Digest) -> Handled {
    // Per repository, never registry-wide: a blob mounted into two repositories
    // and deleted from one must still be servable from the other, and the suite
    // checks exactly that after a cross-repo mount. Whether the bytes are ever
    // reclaimed is purge's business.
    ctx.registry()
        .delete_blob(name, digest)
        .await
        .map_err(ops_error)?;
    Ok(empty_with_length(StatusCode::ACCEPTED, 0, Vec::new()))
}
