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
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;
use crate::counters::PullCounters;
use crate::error::{ApiError, ErrorCode};
use crate::handlers;
use crate::handlers::api::ApiEndpoint;
use crate::query;
use crate::reference::valid_name;
use crate::seam::Registry;
use crate::ui;

/// Optional, and sent anyway: it costs one header and placates tooling old
/// enough to look for it. The companion `Docker-Upload-UUID` is not sent - it
/// is redundant with `Location`, which clients MUST use verbatim regardless.
pub const API_VERSION_HEADER: HeaderName =
    HeaderName::from_static("docker-distribution-api-version");

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<dyn Registry>,
    pub config: Arc<ServerConfig>,
    /// Where a served pull is counted.
    ///
    /// Above the seam on purpose: what is counted is an HTTP fact - a `GET` and
    /// not a `HEAD`, and the bytes that reached the socket rather than the ones
    /// a range asked for - so the handlers are where it is known. Draining this
    /// into the store is `backend.rs`'s job.
    pub counters: Arc<PullCounters>,
}

impl AppState {
    /// A registry that counts nothing.
    ///
    /// The default because every embedding that is not `summ serve` - the tests
    /// above all - has no flush task behind it, and counters that accumulate
    /// with nothing draining them are a leak rather than a feature.
    pub fn new(registry: Arc<dyn Registry>, config: ServerConfig) -> Self {
        AppState::with_counters(registry, config, Arc::new(PullCounters::disabled()))
    }

    pub fn with_counters(
        registry: Arc<dyn Registry>,
        config: ServerConfig,
        counters: Arc<PullCounters>,
    ) -> Self {
        AppState {
            registry,
            config: Arc::new(config),
            counters,
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

/// Split an `/api/v1/` path into an [`ApiEndpoint`].
///
/// Unlike [`route`], this does **not** match a suffix - and that is the whole
/// design. A repository name may contain `/`, so a nested
/// `/repositories/<name>/tags` is ambiguous: a registry holding both `foo` and
/// `foo/tags` has one path that means two things, and whichever way it is
/// resolved the other repository becomes unreachable or, worse, silently
/// answers with the first one's data. `/v2/` lives with that because its shapes
/// are fixed by the spec; this API is ours, so it is built out of the
/// ambiguity instead.
///
/// Each collection is therefore its own top-level resource and the name is
/// everything after it, to the end of the path. A single manifest is
/// `<name>@<reference>`, split at the last `@`, which is unambiguous because
/// `@` appears in neither the name grammar nor the tag grammar nor a digest.
pub fn api_route(path: &str) -> Result<ApiEndpoint, RouteError> {
    let rest = path.strip_prefix("/api/v1/").ok_or(RouteError::NoMatch)?;
    let (resource, remainder) = match rest.split_once('/') {
        Some((resource, remainder)) => (resource, remainder),
        None => (rest, ""),
    };

    // Percent-decoded per segment, after the split, so an encoded `%2F` inside
    // a tag can never become a path separator.
    let decode = |raw: &str| {
        raw.split('/')
            .map(query::path_decode)
            .collect::<Vec<_>>()
            .join("/")
    };
    let name_of = |raw: &str| {
        let name = decode(raw);
        if name.is_empty() {
            Err(RouteError::NoMatch)
        } else if valid_name(&name) {
            Ok(name)
        } else {
            Err(RouteError::InvalidName(name))
        }
    };

    let remainder = remainder.strip_suffix('/').unwrap_or(remainder);
    match resource {
        "repositories" if remainder.is_empty() => Ok(ApiEndpoint::Repositories),
        "repositories" => Ok(ApiEndpoint::Repository {
            name: name_of(remainder)?,
        }),
        "tags" => Ok(ApiEndpoint::Tags {
            name: name_of(remainder)?,
        }),
        "manifests" => match remainder.rsplit_once('@') {
            Some((name, reference)) => Ok(ApiEndpoint::Manifest {
                name: name_of(name)?,
                reference: query::path_decode(reference),
            }),
            None => Ok(ApiEndpoint::Manifests {
                name: name_of(remainder)?,
            }),
        },
        // A reference is mandatory here, unlike `manifests`: there is no
        // whole-repository history collection. `H` and `J` are both scoped to
        // one tag or one manifest, and a repo-wide scan across every tag's
        // events is exactly the unbounded read this API does not offer.
        "tag-history" => match remainder.rsplit_once('@') {
            Some((name, reference)) => Ok(ApiEndpoint::TagHistory {
                name: name_of(name)?,
                reference: query::path_decode(reference),
            }),
            None => Err(RouteError::NoMatch),
        },
        // A reference is optional here, unlike `tag-history`: the repository is
        // itself a counter scope, and the only one carrying blob traffic. The
        // three scopes are separate series maintained on write, so this is not
        // a rollup of the per-manifest ones - see `PLAN.md`.
        "pull-counts" => match remainder.rsplit_once('@') {
            Some((name, reference)) => Ok(ApiEndpoint::PullCounts {
                name: name_of(name)?,
                reference: Some(query::path_decode(reference)),
            }),
            None => Ok(ApiEndpoint::PullCounts {
                name: name_of(remainder)?,
                reference: None,
            }),
        },
        _ => Err(RouteError::NoMatch),
    }
}

/// Build the application.
///
/// The middleware stack is short on purpose. There is no compression layer
/// anywhere - a blob's digest is over its plaintext bytes, so transforming a
/// body breaks it, and a layer scoped "not near `/blobs/`" is one refactor away
/// from being wrong. There is no rate limiter either; see
/// [`crate::error::ErrorCode::TooManyRequests`].
///
/// The one thing that *is* a layer rather than a handler is authentication, and
/// it is a layer precisely because there is no route it does not apply to: the
/// `/v2/` surface, the discovery API and the UI are one registry and one
/// credential. Put it in the handlers instead and protecting the next endpoint
/// becomes something a person has to remember. It sits inside [`TraceLayer`],
/// so a rejected request is still traced, and outside the routes, so a `401`
/// costs no routing and no state lookup.
pub fn router(state: AppState) -> Router {
    let auth = state.config.auth.is_enabled();
    let router = Router::new()
        .route("/v2", any(dispatch))
        .route("/v2/", any(dispatch))
        .route("/v2/{*rest}", any(dispatch))
        .route("/api/v1/{*rest}", any(dispatch_api))
        // Everything else is the web UI: its own assets by path, and the shell
        // for any other route so a deep link into a repository page survives a
        // reload. A path under `/api/` that matched no route above must not be
        // swallowed by that - see `fallback`.
        .fallback(fallback)
        // Blob bodies are gigabytes; axum's 2 MB default would reject them.
        // Manifests get their own limit in the handler, because exceeding it
        // has to be a `413` with a spec error body rather than a bare status.
        .layer(DefaultBodyLimit::disable())
        .layer(SetResponseHeaderLayer::if_not_present(
            API_VERSION_HEADER,
            HeaderValue::from_static("registry/2.0"),
        ));

    // Attached only when it would do something. An always-present layer that
    // matches `Anonymous` and calls `next` is a clone of the request extensions
    // and a future per request bought for nothing, and the default deployment
    // is the anonymous one.
    let router = if auth {
        router.layer(middleware::from_fn_with_state(state.clone(), authenticate))
    } else {
        router
    };

    router.layer(TraceLayer::new_for_http()).with_state(state)
}

/// Reject a request that has no credential for what it is about to do.
///
/// Note what is *not* here: nothing reads the body, and nothing routes. A
/// rejected push is rejected before its first byte is read, which matters for
/// the shape this registry is built around - a blob `PUT` is gigabytes, and a
/// `401` that has to drain one is a denial-of-service surface rather than an
/// answer. axum drops the body with the request, and hyper resets the stream.
async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match state
        .config
        .auth
        .authorize(request.method(), request.headers())
    {
        Ok(()) => next.run(request).await,
        Err(err) => err.into_api_error().into_response(),
    }
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

async fn dispatch_api(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    match api_route(&path) {
        Ok(endpoint) => {
            let (parts, _) = request.into_parts();
            let ctx = handlers::Ctx {
                state,
                method: parts.method,
                path: parts.uri.path().to_owned(),
                query: query::pairs(parts.uri.query().unwrap_or("")),
                headers: parts.headers,
            };
            handlers::api::handle(&ctx, endpoint)
                .await
                .unwrap_or_else(IntoResponse::into_response)
        }
        Err(RouteError::InvalidName(name)) => ApiError::new(ErrorCode::NameInvalid)
            .with_detail(name)
            .into_response(),
        Err(RouteError::NoMatch) => ApiError::new(ErrorCode::NameUnknown)
            .with_message("unknown endpoint")
            .with_detail(path)
            .into_response(),
    }
}

/// Anything that matched no route above.
///
/// Two populations, and conflating them would be the bug: a machine asking for
/// an API path that does not exist needs a JSON error, and a browser asking for
/// `/r/library/nginx` needs the UI shell, because the UI routes client-side and
/// a deep link has to survive a reload. So `/api/` and `/v2/` keep the error and
/// everything else gets the shell.
async fn fallback(request: Request) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") || path.starts_with("/v2/") {
        return ApiError::new(ErrorCode::NameUnknown)
            .with_message("not found")
            .with_detail(path.to_owned())
            .into_response();
    }
    ui::serve(request.method(), path)
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

    /// The `@` split is what makes a name containing `/` safe here: a
    /// repository named `demo/app` and a tag are one path, unambiguously,
    /// because `@` is in neither the name grammar nor the tag grammar.
    #[test]
    fn tag_history_splits_the_reference_off_the_end() {
        assert_eq!(
            api_route("/api/v1/tag-history/demo/app@latest"),
            Ok(ApiEndpoint::TagHistory {
                name: "demo/app".to_owned(),
                reference: "latest".to_owned(),
            })
        );
        assert_eq!(
            api_route("/api/v1/tag-history/demo/app@sha256:abcd"),
            Ok(ApiEndpoint::TagHistory {
                name: "demo/app".to_owned(),
                reference: "sha256:abcd".to_owned(),
            })
        );
        // No whole-repository history collection: `H` and `J` are both scoped
        // to one tag or one manifest, so a bare name has nothing to answer.
        assert_eq!(
            api_route("/api/v1/tag-history/demo/app"),
            Err(RouteError::NoMatch)
        );
        assert_eq!(api_route("/api/v1/tag-history"), Err(RouteError::NoMatch));
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
