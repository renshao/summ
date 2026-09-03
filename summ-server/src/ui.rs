//! The built-in web UI: four files, compiled into the binary.
//!
//! The UI is a headline feature rather than an extra, and the reason is the
//! honesty check it performs: a catalogue browser that stays responsive over
//! ten million repositories is only possible if the discovery API is genuinely
//! cursor-paged, so anything that accidentally scans the whole keyspace shows
//! up here first. That is also why it ships in the binary - a UI that needs a
//! separate deploy is one that stops being run.
//!
//! There is no build step and no framework. Assets are `include_str!`d, which
//! means `cargo build` is the whole pipeline and there is no `node_modules` in
//! the dependency graph of a container registry. It also means no CDN: a
//! registry is exactly the kind of thing that runs in an air-gapped network,
//! and a UI that needs the public internet to render would be useless there.
//!
//! Routing is client-side, so every path that is not an asset serves the shell
//! and the page works out what to render. `/v2/` and `/api/` are excluded
//! before this module is reached - see [`crate::app::fallback`].

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const LOGO_SVG: &str = include_str!("../ui/logo.svg");

/// Assets are served from a fixed table rather than from a directory walk.
/// There is no filesystem to traverse at runtime, so there is also no path that
/// could escape one.
fn asset(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/app.css" => Some(("text/css; charset=utf-8", APP_CSS)),
        "/app.js" => Some(("text/javascript; charset=utf-8", APP_JS)),
        // The favicon. Served as a file rather than inlined as a `data:` URI
        // in the shell so the mark has exactly one definition to edit.
        "/logo.svg" => Some(("image/svg+xml; charset=utf-8", LOGO_SVG)),
        _ => None,
    }
}

pub fn serve(method: &Method, path: &str) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response();
    }

    let (content_type, body) = asset(path).unwrap_or(("text/html; charset=utf-8", INDEX_HTML));
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len())
        // The assets change only when the binary does, but they are served
        // under stable names, so a cached copy would survive an upgrade. The
        // registry answers this in microseconds from memory; revalidating is
        // cheaper than being wrong.
        .header(header::CACHE_CONTROL, "no-cache");

    if method == Method::HEAD {
        return response
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    response
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
