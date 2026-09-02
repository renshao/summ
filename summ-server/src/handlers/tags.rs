//! end-8: `GET /v2/<name>/tags/list`.
//!
//! Three MUSTs live here.
//!
//! **Order.** The spec says tags "MUST be in lexical (i.e. case-insensitive
//! alphanumeric order) or 'ASCIIbetical' (Go's `sort.Strings`) order". Those
//! two descriptions are not the same thing, and the conformance suite assumes
//! the second, so the ordering is raw byte order. That is free: the `T <repo>
//! <tag>` key range is already byte-ordered, so the scan arrives sorted.
//!
//! **`?last=` is exclusive.** Results begin strictly after the named tag, and
//! `last` must be a real tag rather than an index - which is what lets the
//! cursor be a seek with no server-side state.
//!
//! **`?n=0` returns an empty list and no `Link`**, called out explicitly by the
//! spec.

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use serde::Serialize;

use super::{build, method_not_allowed, ops_error, Ctx, Handled};
use crate::error::ApiError;
use crate::pagination;

#[derive(Serialize)]
struct TagsBody<'a> {
    name: &'a str,
    tags: Vec<String>,
}

pub async fn handle(ctx: &Ctx, name: &str) -> Handled {
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        return Err(method_not_allowed("GET, HEAD"));
    }

    let params = pagination::parse(&ctx.query, ctx.config())?;
    // Called even for `n=0`, because an unknown repository must still be a
    // `404 NAME_UNKNOWN` rather than an empty list.
    let page = ctx
        .registry()
        .tags(name, params.last.as_deref(), params.limit)
        .await
        .map_err(ops_error)?;

    let link = (!params.explicit_zero && page.more)
        .then(|| page.items.last())
        .flatten()
        .and_then(|last| {
            pagination::link_next(&format!("/v2/{name}/tags/list"), last, params.limit)
        });

    let body = TagsBody {
        name,
        tags: page.items,
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
