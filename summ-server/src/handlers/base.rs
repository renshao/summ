//! end-1: `GET /v2/`.
//!
//! The spec requires only a `200`; the body is unspecified. Every registry
//! returns `{}` with `Content-Type: application/json`, and some client
//! tooling parses it, so summ returns exactly that.
//!
//! The spec notes this endpoint MAY be used for authentication - a `401` here
//! is how a client discovers the token endpoint. Auth is Phase 6, so the
//! handler is unconditional for now.

use axum::body::Body;
use axum::http::{header, Method, StatusCode};

use super::{build, empty_with_length, method_not_allowed, Ctx, Handled};

const BODY: &[u8] = b"{}";

pub fn handle(ctx: &Ctx) -> Handled {
    match ctx.method {
        Method::GET => Ok(build(
            axum::http::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, BODY.len()),
            Body::from(BODY),
        )),
        Method::HEAD => Ok(empty_with_length(
            StatusCode::OK,
            BODY.len() as u64,
            vec![(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )],
        )),
        _ => Err(method_not_allowed("GET, HEAD")),
    }
}
