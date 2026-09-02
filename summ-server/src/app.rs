//! The router and the `/v2/` path dispatcher.
//!
//! # Why the routing is hand-written
//!
//! A repository name may contain `/`: `foo/bar/baz` is one name, not three path
//! segments. So `/v2/{name}/blobs/{digest}` is not expressible in axum's
//! router at all, and the operation is identified by a *suffix* of the path
//! rather than a prefix.
//!
//! The two known answers are to generate one route per name depth - Trow
//! generates seven with a macro pair, which caps names at seven components -
//! or to take a single catch-all and split the suffix by hand. summ takes the
//! second: it has no depth limit, it puts the whole route table in one
//! readable function, and [`route`] is then a pure `&str -> Endpoint` function
//! that can be unit-tested without a server.
//!
//! Note also that axum 0.8 changed path syntax from `/:param` to `/{param}`;
//! the old form is not deprecated, it panics at startup.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;
use crate::error::{ApiError, ErrorCode};
use crate::handlers;
use crate::query;
use crate::reference::valid_name;
use crate::seam::Registry;

/// Optional, and sent anyway: it costs one header and placates tooling old
/// enough to look for it. The companion `Docker-Upload-UUID` is not sent - it
/// is redundant with `Location`, which clients MUST use verbatim regardless.
pub const API_VERSION_HEADER: HeaderName =
    HeaderName::from_static("docker-distribution-api-version");

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<dyn Registry>,
    pub config: Arc<ServerConfig>,
}

impl AppState {
    pub fn new(registry: Arc<dyn Registry>, config: ServerConfig) -> Self {
        AppState {
            registry,
            config: Arc::new(config),
        }
    }
}

/// One `/v2/` operation, with the repository name already split out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `GET /v2/` - end-1.
    Base,
    /// `GET /v2/_catalog`. Not a spec endpoint: it was removed before v1.0.0
    /// and the conformance suite never calls it. Implemented because every
    /// client uses it, and paged over the name-ordered range like everything
    /// else.
    Catalog,
    /// `/v2/<name>/tags/list` - end-8.
    TagList { name: String },
    /// `/v2/<name>/manifests/<reference>` - end-3, end-7, end-9.
    Manifest { name: String, reference: String },
    /// `/v2/<name>/blobs/<digest>` - end-2, end-10.
    Blob { name: String, digest: String },
    /// `POST /v2/<name>/blobs/uploads/` - end-4, end-11.
    Uploads { name: String },
    /// `<blob-push-location>` - end-5, end-6, end-13, end-14.
    Upload { name: String, id: String },
    /// `/v2/<name>/referrers/<digest>` - end-12.
    Referrers { name: String, digest: String },
}

/// Why a path did not become an [`Endpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// The shape matched but the repository name violated the grammar.
    InvalidName(String),
    /// No endpoint has this shape.
    NoMatch,
}

/// Split a request path into an [`Endpoint`].
///
/// The suffix is matched from the right, longest form first, because the name
/// occupies everything to the left of it. A repository legitimately called
/// `foo/blobs` is therefore routable: `/v2/foo/blobs/manifests/v1` matches the
/// `manifests/<ref>` suffix and leaves `foo/blobs` as the name.
///
/// Segments are percent-decoded individually *after* splitting on `/`, so an
/// encoded `%2F` inside a tag can never be mistaken for a path separator.
///
/// A path whose *shape* matches an endpoint but whose name violates the grammar
/// is [`RouteError::InvalidName`], not [`RouteError::NoMatch`]: the reference
/// implementation lets its router answer such a request with a plain-text
/// `404`, but the spec has `NAME_INVALID` for exactly this and a client can act
/// on the difference.
pub fn route(path: &str) -> Result<Endpoint, RouteError> {
    let Some(rest) = path.strip_prefix("/v2") else {
        return Err(RouteError::NoMatch);
    };
    let rest = match rest {
        "" | "/" => return Ok(Endpoint::Base),
        other => other.strip_prefix('/').ok_or(RouteError::NoMatch)?,
    };

    let mut segments: Vec<String> = rest.split('/').map(query::path_decode).collect();
    // The name is everything to the left of the matched suffix, joined back
    // together. It is validated here so an `Endpoint` always carries a name
    // that satisfied the grammar.
    fn name_of(segments: &[String], count: usize) -> Result<String, RouteError> {
        let name = segments
            .get(..count)
            .filter(|s| !s.is_empty())
            .ok_or(RouteError::NoMatch)?
            .join("/");
        if valid_name(&name) {
            Ok(name)
        } else {
            Err(RouteError::InvalidName(name))
        }
    }

    // `POST /v2/<name>/blobs/uploads/` carries a trailing slash. Accept it with
    // or without, as every registry does.
    if segments.last().is_some_and(String::is_empty) {
        segments.pop();
    }
    if segments.iter().any(String::is_empty) {
        return Err(RouteError::NoMatch);
    }

    let n = segments.len();
    if n == 1 && segments[0] == "_catalog" {
        return Ok(Endpoint::Catalog);
    }

    // Longest suffix first: `blobs/uploads/<id>` before `blobs/<digest>`,
    // otherwise an upload id would be read as a digest.
    if n >= 4 && segments[n - 3] == "blobs" && segments[n - 2] == "uploads" {
        return Ok(Endpoint::Upload {
            name: name_of(&segments, n - 3)?,
            id: segments[n - 1].clone(),
        });
    }
    if n >= 3 && segments[n - 2] == "blobs" && segments[n - 1] == "uploads" {
        return Ok(Endpoint::Uploads {
            name: name_of(&segments, n - 2)?,
        });
    }
    if n >= 3 && segments[n - 2] == "manifests" {
        let reference = segments.pop().ok_or(RouteError::NoMatch)?;
        return Ok(Endpoint::Manifest {
            name: name_of(&segments, n - 2)?,
            reference,
        });
    }
    if n >= 3 && segments[n - 2] == "blobs" {
        let digest = segments.pop().ok_or(RouteError::NoMatch)?;
        return Ok(Endpoint::Blob {
            name: name_of(&segments, n - 2)?,
            digest,
        });
    }
    if n >= 3 && segments[n - 2] == "tags" && segments[n - 1] == "list" {
        return Ok(Endpoint::TagList {
            name: name_of(&segments, n - 2)?,
        });
    }
    if n >= 3 && segments[n - 2] == "referrers" {
        let digest = segments.pop().ok_or(RouteError::NoMatch)?;
        return Ok(Endpoint::Referrers {
            name: name_of(&segments, n - 2)?,
            digest,
        });
    }
    Err(RouteError::NoMatch)
}

/// Build the application.
///
/// The middleware stack is short on purpose. There is no compression layer
/// anywhere - a blob's digest is over its plaintext bytes, so transforming a
/// body breaks it, and a layer scoped "not near `/blobs/`" is one refactor away
/// from being wrong. There is no rate limiter either; see
/// [`crate::error::ErrorCode::TooManyRequests`].
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v2", any(dispatch))
        .route("/v2/", any(dispatch))
        .route("/v2/{*rest}", any(dispatch))
        .fallback(fallback)
        // Blob bodies are gigabytes; axum's 2 MB default would reject them.
        // Manifests get their own limit in the handler, because exceeding it
        // has to be a `413` with a spec error body rather than a bare status.
        .layer(DefaultBodyLimit::disable())
        .layer(SetResponseHeaderLayer::if_not_present(
            API_VERSION_HEADER,
            HeaderValue::from_static("registry/2.0"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn dispatch(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    match route(&path) {
        Ok(endpoint) => handlers::handle(state, endpoint, request).await,
        Err(RouteError::InvalidName(name)) => ApiError::new(ErrorCode::NameInvalid)
            .with_detail(name)
            .into_response(),
        // A `/v2/` path that matches no endpoint at all. Any 4XX body is
        // permitted here, but a spec-shaped one is strictly more useful than
        // the reference implementation's plain-text router `404`.
        Err(RouteError::NoMatch) => ApiError::new(ErrorCode::NameUnknown)
            .with_message("unknown repository or endpoint")
            .with_detail(path)
            .into_response(),
    }
}

/// Anything outside `/v2/`. The UI and the extension API will claim their own
/// prefixes later; until then this is an honest `404`.
async fn fallback(request: Request) -> Response {
    ApiError::new(ErrorCode::NameUnknown)
        .with_message("not found")
        .with_detail(request.uri().path().to_owned())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, reference: &str) -> Result<Endpoint, RouteError> {
        Ok(Endpoint::Manifest {
            name: name.to_owned(),
            reference: reference.to_owned(),
        })
    }

    #[test]
    fn base_matches_with_and_without_a_trailing_slash() {
        assert_eq!(route("/v2/"), Ok(Endpoint::Base));
        assert_eq!(route("/v2"), Ok(Endpoint::Base));
    }

    #[test]
    fn catalog_is_not_confused_with_a_repository() {
        assert_eq!(route("/v2/_catalog"), Ok(Endpoint::Catalog));
        // `_catalog` fails the name grammar, so there is no ambiguity to
        // resolve: it could never have been a repository.
        assert_eq!(
            route("/v2/_catalog/tags/list"),
            Err(RouteError::InvalidName("_catalog".to_owned()))
        );
    }

    #[test]
    fn repository_names_may_contain_slashes() {
        assert_eq!(route("/v2/foo/manifests/v1"), manifest("foo", "v1"));
        assert_eq!(route("/v2/foo/bar/manifests/v1"), manifest("foo/bar", "v1"));
        assert_eq!(
            route("/v2/foo/bar/baz/manifests/v1"),
            manifest("foo/bar/baz", "v1")
        );
        assert_eq!(
            route("/v2/a/b/c/d/e/f/g/h/i/manifests/v1"),
            manifest("a/b/c/d/e/f/g/h/i", "v1"),
            "there is no depth limit, unlike a per-depth route table"
        );
    }

    #[test]
    fn a_repository_may_be_called_blobs_or_manifests() {
        assert_eq!(
            route("/v2/foo/blobs/manifests/v1"),
            manifest("foo/blobs", "v1")
        );
        assert_eq!(
            route("/v2/foo/manifests/tags/list"),
            Ok(Endpoint::TagList {
                name: "foo/manifests".to_owned()
            })
        );
    }

    #[test]
    fn the_upload_suffix_wins_over_the_blob_suffix() {
        assert_eq!(
            route("/v2/foo/blobs/uploads/"),
            Ok(Endpoint::Uploads {
                name: "foo".to_owned()
            })
        );
        assert_eq!(
            route("/v2/foo/blobs/uploads"),
            Ok(Endpoint::Uploads {
                name: "foo".to_owned()
            })
        );
        assert_eq!(
            route("/v2/foo/bar/blobs/uploads/abc-123"),
            Ok(Endpoint::Upload {
                name: "foo/bar".to_owned(),
                id: "abc-123".to_owned()
            })
        );
    }

    #[test]
    fn blobs_tags_and_referrers_route() {
        assert_eq!(
            route("/v2/foo/blobs/sha256:abcd"),
            Ok(Endpoint::Blob {
                name: "foo".to_owned(),
                digest: "sha256:abcd".to_owned()
            })
        );
        assert_eq!(
            route("/v2/foo/tags/list"),
            Ok(Endpoint::TagList {
                name: "foo".to_owned()
            })
        );
        assert_eq!(
            route("/v2/foo/referrers/sha256:abcd"),
            Ok(Endpoint::Referrers {
                name: "foo".to_owned(),
                digest: "sha256:abcd".to_owned()
            })
        );
    }

    #[test]
    fn a_name_outside_the_grammar_is_distinguished_from_no_route() {
        // The shape matched, so this is `NAME_INVALID` rather than a bare 404.
        assert_eq!(
            route("/v2/FOO/manifests/v1"),
            Err(RouteError::InvalidName("FOO".to_owned()))
        );
        assert_eq!(
            route("/v2/foo-/manifests/v1"),
            Err(RouteError::InvalidName("foo-".to_owned()))
        );
        // An empty segment is not a name at all.
        assert_eq!(route("/v2//manifests/v1"), Err(RouteError::NoMatch));
    }

    #[test]
    fn unknown_shapes_do_not_route() {
        assert_eq!(route("/v2/foo"), Err(RouteError::NoMatch));
        assert_eq!(route("/v2/foo/bar"), Err(RouteError::NoMatch));
        assert_eq!(route("/v2/foo/whatever/v1"), Err(RouteError::NoMatch));
        assert_eq!(route("/v1/foo/manifests/v1"), Err(RouteError::NoMatch));
        assert_eq!(route("/"), Err(RouteError::NoMatch));
    }

    #[test]
    fn segments_are_decoded_after_splitting() {
        // `%2F` inside a reference stays inside the reference. It will then be
        // rejected by the tag grammar, which is the correct outcome - what
        // matters is that it never became a path separator.
        assert_eq!(route("/v2/foo/manifests/a%2Fb"), manifest("foo", "a/b"));
    }
}
