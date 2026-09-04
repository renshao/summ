//! end-4, end-5, end-6, end-11, end-13, end-14: the blob upload state machine.
//!
//! Four flows share this code, and all four are exercised by the conformance
//! suite:
//!
//! | Flow | Shape |
//! |---|---|
//! | A, monolithic | `POST` → `202`, then `PUT ?digest=` → `201` |
//! | B, single POST | `POST ?digest=` with the body → `201` |
//! | C, streamed | `POST`, `PATCH` with **no** `Content-Range`, `PUT ?digest=` |
//! | D, chunked | `POST`, repeated `PATCH` with `Content-Range`, `PUT ?digest=` |
//!
//! The two rules that decide whether this works:
//!
//! **A `PATCH` carrying neither `Content-Range` nor `Content-Length` is a
//! stream, and it is mandatory.** It is what `docker push` and BuildKit
//! actually send - chunked transfer encoding, no range. A registry that
//! requires `Content-Range` on `PATCH` fails every streamed-upload check. The
//! range is validated only when *both* headers are present; exactly one of them
//! is ambiguous and is treated as a stream.
//!
//! **An out-of-order chunk is a `416` and MUST leave the session untouched.**
//! The offset is therefore checked before a single byte is handed to the
//! storage layer, and the client recovers with the end-13 status `GET`.

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode};
use summ_core::Digest;
use uuid::Uuid;

use super::{
    digest_header, empty_with_length, method_not_allowed, ops_error, Ctx, Handled,
    DOCKER_CONTENT_DIGEST, OCI_CHUNK_MIN_LENGTH,
};
use crate::error::{ApiError, ErrorCode};
use crate::range::{parse_chunk_range, upload_range_header, ChunkRangeError};
use crate::reference::{parse_digest, valid_name};
use crate::seam::UploadBody;

/// `POST /v2/<name>/blobs/uploads/`.
pub async fn create(ctx: &Ctx, name: &str, body: Body) -> Handled {
    if ctx.method != Method::POST {
        return Err(method_not_allowed("POST"));
    }

    if let Some(raw) = ctx.param("mount") {
        if let Some(response) = mount(ctx, name, raw).await? {
            return Ok(response);
        }
        // Falls through to opening an ordinary session. A refused mount is a
        // `202`, not an error: the spec says a registry that cannot mount
        // SHOULD answer `202` and let the client upload normally.
    }

    if let Some(raw) = ctx.param("digest") {
        return single_post(ctx, name, raw, body).await;
    }

    let algorithm = digest_algorithm(ctx)?;
    // Minted here and passed down, so the resulting `WriteBatch` contains no
    // engine-generated value and means the same thing if it is replayed.
    let id = Uuid::new_v4().to_string();
    ctx.registry()
        .create_upload(name, &id, algorithm)
        .await
        .map_err(ops_error)?;

    let mut headers = vec![
        (header::LOCATION, location(name, &id)),
        (header::RANGE, ascii(&upload_range_header(0))),
    ];
    if let Some(min) = ctx.config().chunk_min_length {
        headers.push((OCI_CHUNK_MIN_LENGTH, ascii(&min.to_string())));
    }
    Ok(empty_with_length(StatusCode::ACCEPTED, 0, headers))
}

/// `?mount=<digest>&from=<other_name>`, end-11.
///
/// `Ok(None)` means "not mounted, open a session instead". `from` is optional:
/// the spec allows anonymous mount, and with a registry-wide blob record
/// answering "is this blob present anywhere" is a single point lookup - the
/// cheapest possible push.
async fn mount(
    ctx: &Ctx,
    name: &str,
    raw: &str,
) -> Result<Option<axum::response::Response>, ApiError> {
    let Ok(digest) = parse_digest(raw) else {
        // A malformed mount digest is not worth failing the push over; the
        // client is told to upload normally, which always works.
        return Ok(None);
    };
    // A `from` outside the name grammar is ignored rather than rejected, for
    // the same reason.
    let from = ctx.param("from").filter(|f| valid_name(f));

    let mounted = ctx
        .registry()
        .mount_blob(name, &digest, from)
        .await
        .map_err(ops_error)?;
    if !mounted {
        return Ok(None);
    }

    Ok(Some(empty_with_length(
        StatusCode::CREATED,
        0,
        vec![
            (header::LOCATION, blob_location(name, &digest)),
            (DOCKER_CONTENT_DIGEST, digest_header(&digest)),
        ],
    )))
}

/// `POST /v2/<name>/blobs/uploads/?digest=<digest>` with the whole blob, end-4b.
///
/// Optional in the spec, and the reference implementation does not do it, which
/// is why fifteen conformance leaves read `Skip` against it. It is one round
/// trip instead of two on the hot push path and is nearly free given the
/// upload is stream-hashed anyway.
async fn single_post(ctx: &Ctx, name: &str, raw: &str, body: Body) -> Handled {
    let digest =
        parse_digest(raw).map_err(|e| ApiError::new(ErrorCode::DigestInvalid).with_detail(e.0))?;
    ctx.registry()
        .put_blob(name, &digest, upload_body(ctx, body, None))
        .await
        .map_err(ops_error)?;

    Ok(empty_with_length(
        StatusCode::CREATED,
        0,
        vec![
            (header::LOCATION, blob_location(name, &digest)),
            (DOCKER_CONTENT_DIGEST, digest_header(&digest)),
        ],
    ))
}

/// Everything addressed at a `<blob-push-location>`: end-5, end-6, end-13,
/// end-14.
pub async fn session(ctx: &Ctx, name: &str, id: &str, body: Body) -> Handled {
    match ctx.method {
        Method::PATCH => patch(ctx, name, id, body).await,
        Method::PUT => finish(ctx, name, id, body).await,
        Method::GET => status(ctx, name, id).await,
        Method::DELETE => cancel(ctx, name, id).await,
        _ => Err(method_not_allowed("GET, PATCH, PUT, DELETE")),
    }
}

async fn patch(ctx: &Ctx, name: &str, id: &str, body: Body) -> Handled {
    let offset = ctx
        .registry()
        .upload_offset(name, id)
        .await
        .map_err(ops_error)?;

    // Everything the `416` depends on is decided here, before a byte of the
    // body is touched: a rejected chunk must leave the session byte-identical.
    let declared = validate_chunk(ctx, offset)?;

    let new_offset = ctx
        .registry()
        .append_upload(name, id, offset, upload_body(ctx, body, declared))
        .await
        .map_err(ops_error)?;

    Ok(empty_with_length(
        StatusCode::ACCEPTED,
        0,
        vec![
            (header::LOCATION, location(name, id)),
            (header::RANGE, ascii(&upload_range_header(new_offset))),
        ],
    ))
}

async fn finish(ctx: &Ctx, name: &str, id: &str, body: Body) -> Handled {
    // The `?digest=` parameter is the only place a whole-blob digest is ever
    // verified: a `PATCH` cannot verify one because it has not seen the end.
    let Some(raw) = ctx.param("digest") else {
        return Err(ApiError::new(ErrorCode::DigestInvalid)
            .with_message("digest query parameter is required to close an upload"));
    };
    let digest =
        parse_digest(raw).map_err(|e| ApiError::new(ErrorCode::DigestInvalid).with_detail(e.0))?;

    let offset = ctx
        .registry()
        .upload_offset(name, id)
        .await
        .map_err(ops_error)?;

    // A closing `PUT` may carry a final chunk, and the spec is explicit that an
    // out-of-order final chunk is a `416` just like any other.
    let declared = validate_chunk(ctx, offset)?;

    ctx.registry()
        .finish_upload(name, id, offset, upload_body(ctx, body, declared), &digest)
        .await
        .map_err(ops_error)?;

    // `Location` here is a **pullable blob URL**, not the upload URL. The suite
    // immediately `GET`s it and byte-compares, so returning the session URL is
    // a silent failure.
    Ok(empty_with_length(
        StatusCode::CREATED,
        0,
        vec![
            (header::LOCATION, blob_location(name, &digest)),
            (DOCKER_CONTENT_DIGEST, digest_header(&digest)),
        ],
    ))
}

/// end-13. `204 No Content`, not `200` - this is the documented recovery after
/// a `416`, and the client reads the surviving offset out of `Range`.
async fn status(ctx: &Ctx, name: &str, id: &str) -> Handled {
    let offset = ctx
        .registry()
        .upload_offset(name, id)
        .await
        .map_err(ops_error)?;
    Ok(empty_with_length(
        StatusCode::NO_CONTENT,
        0,
        vec![
            (header::LOCATION, location(name, id)),
            (header::RANGE, ascii(&upload_range_header(offset))),
        ],
    ))
}

/// end-14.
async fn cancel(ctx: &Ctx, name: &str, id: &str) -> Handled {
    ctx.registry()
        .cancel_upload(name, id)
        .await
        .map_err(ops_error)?;
    Ok(empty_with_length(StatusCode::NO_CONTENT, 0, Vec::new()))
}

/// Validate a chunk's `Content-Range` against the session's committed offset.
///
/// Returns the declared chunk length when this request is a chunk, or `None`
/// when it is a stream. Everything here happens **before** any byte is written,
/// because a `416` must leave the session byte-identical.
fn validate_chunk(ctx: &Ctx, offset: u64) -> Result<Option<u64>, ApiError> {
    let content_range = ctx.header(header::CONTENT_RANGE);
    let content_length = ctx
        .header(header::CONTENT_LENGTH)
        .and_then(|v| v.parse::<u64>().ok());

    // Both present: a chunk. Neither, or only one: a stream. Treating exactly
    // one as a stream matches the reference implementation and is the only
    // reading under which the streamed flow can work at all.
    let (Some(raw), Some(length)) = (content_range, content_length) else {
        return Ok(None);
    };

    let range = parse_chunk_range(raw).map_err(|e| match e {
        ChunkRangeError::Malformed(detail) => ApiError::new(ErrorCode::BlobUploadInvalid)
            .with_message("malformed Content-Range")
            .with_detail(detail),
        ChunkRangeError::Inverted => ApiError::new(ErrorCode::BlobUploadInvalid)
            .with_message("Content-Range start is after its end")
            .with_detail(raw.to_owned()),
    })?;

    if range.size() != length {
        return Err(size_mismatch(range.size(), length));
    }
    if range.start != offset {
        // `BLOB_UPLOAD_INVALID`, not the reference implementation's invented
        // `RANGE_INVALID`, which is outside the spec's fourteen codes.
        return Err(ApiError::new(ErrorCode::BlobUploadInvalid)
            .with_status(StatusCode::RANGE_NOT_SATISFIABLE)
            .with_message("chunk is out of order")
            .with_detail(format!(
                "expected the chunk to start at {offset}, got {}",
                range.start
            )));
    }
    Ok(Some(length))
}

/// `?digest-algorithm=`, end-4c.
///
/// It selects which hasher the session carries, and therefore which algorithm
/// the committed blob is addressed under. The reference implementation ignores
/// it and rehashes everything as sha256, which is the root cause of most of its
/// sha512 conformance failures.
fn digest_algorithm(ctx: &Ctx) -> Result<&str, ApiError> {
    match ctx.param("digest-algorithm") {
        None | Some("") => Ok("sha256"),
        Some(algorithm @ ("sha256" | "sha512")) => Ok(algorithm),
        Some(other) => Err(ApiError::new(ErrorCode::DigestInvalid)
            .with_message("unsupported digest algorithm")
            .with_detail(other.to_owned())),
    }
}

fn size_mismatch(expected: u64, actual: u64) -> ApiError {
    ApiError::new(ErrorCode::SizeInvalid)
        .with_detail(format!("expected {expected} bytes, got {actual}"))
}

/// Hand the body down without reading it.
///
/// The declared length and the per-request ceiling travel with it rather than
/// being checked here, because neither can be checked before the bytes arrive
/// and the whole point is that they are never all here at once.
fn upload_body(ctx: &Ctx, body: Body, declared: Option<u64>) -> UploadBody {
    UploadBody {
        body,
        declared,
        limit: ctx.config().max_upload_bytes,
    }
}

/// The `<blob-push-location>`.
///
/// Relative, which the spec permits, and clients MUST use it verbatim rather
/// than reassembling it - which is what leaves us free to change its shape
/// later, or to hang critical query parameters off it.
fn location(name: &str, id: &str) -> HeaderValue {
    ascii(&format!("/v2/{name}/blobs/uploads/{id}"))
}

/// A pullable blob URL, for the `201` responses.
fn blob_location(name: &str, digest: &Digest) -> HeaderValue {
    ascii(&format!("/v2/{name}/blobs/{digest}"))
}

fn ascii(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}
