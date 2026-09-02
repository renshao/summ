//! `GET /v2/_catalog` - not a spec endpoint.
//!
//! It was removed from the Distribution Spec before v1.0.0 (commit `b4e9833`,
//! "Remove _catalog API, reference as reserved") and survives only as a
//! reserved extension namespace; the conformance suite never calls it. It is
//! implemented anyway because every client and every operator uses it, and
//! because listing repositories quickly is the operation this project exists
//! for.
//!
//! Being outside the spec means its pagination semantics are ours to choose,
//! so they are chosen better than the reference implementation's: an oversized
//! `?n=` is clamped rather than rejected, and `Link` is emitted only when a
//! further page genuinely exists.

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use serde::Serialize;

use super::{build, method_not_allowed, ops_error, Ctx, Handled};
use crate::error::ApiError;
use crate::pagination;

#[derive(Serialize)]
struct CatalogBody {
    repositories: Vec<String>,
}

pub async fn handle(ctx: &Ctx) -> Handled {
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        return Err(method_not_allowed("GET, HEAD"));
    }

    let params = pagination::parse(&ctx.query, ctx.config())?;
    let page = ctx
        .registry()
        .repositories(params.last.as_deref(), params.limit)
        .await
        .map_err(ops_error)?;

    let link = (!params.explicit_zero && page.more)
        .then(|| page.items.last())
        .flatten()
        .and_then(|last| pagination::link_next("/v2/_catalog", last, params.limit));

    let body = CatalogBody {
        repositories: page.items,
    };
    let bytes = serde_json::to_vec(&body).map_err(|e| ApiError::internal(e.to_string()))?;

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, bytes.len());
    if let Some(link) = link {
        builder = builder.header(header::LINK, link);
    }

    if ctx.method == Method::HEAD {
        return Ok(build(builder, Body::empty()));
    }
    Ok(build(builder, Body::from(bytes)))
}
