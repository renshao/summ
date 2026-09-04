//! `/api/v1/` - the discovery API the web UI is built on.
//!
//! None of this is in the Distribution Spec. `GET /v2/_catalog` was removed
//! before v1.0.0 and nothing standard answers "what is in this registry", which
//! is exactly the operation summ exists to make fast. So the shapes here are
//! ours to choose, and the cost is that nothing external validates them - hence
//! `tests/discovery.rs`.
//!
//! Three choices worth stating, because they are the ones a spec would
//! otherwise have made:
//!
//! The route table is flat - `repositories`, `tags`, `manifests`, each taking
//! the repository name as everything after it - because a repository name may
//! contain `/`, and a nested `/repositories/<name>/tags` would mean two things
//! in a registry holding both `foo` and `foo/tags`. [`crate::app::api_route`]
//! has the detail.
//!
//! - **The cursor is in the body, not a `Link` header.** `/v2/` uses `Link`
//!   because a decade of clients parse it. This API has exactly one client
//!   family - `fetch` - and a JSON caller that has already parsed the body
//!   should not have to parse a header to find out whether to ask again.
//! - **A count may be a floor.** The scale target is 10M manifests in one
//!   repository, so counting to completion on a request thread is not an
//!   option, and there is no stored total because maintaining one would be the
//!   read-modify-write on the push path that the key schema exists to avoid.
//!   Every count therefore carries `complete`, and a UI renders a `false` as
//!   `10,000+`.
//! - **Pages here are small by default.** A row on the repository list costs a
//!   bounded count per repository, so the default page is [`DEFAULT_PAGE`] and
//!   the ceiling [`MAX_PAGE`] - both far below the `/v2/` limits, which page
//!   over a single key range and cost one seek.
//!
//! Versioned as `/api/v1/` separately from `/v2/`: the two move for entirely
//! different reasons, and pinning this to the spec's version number would be a
//! promise nobody asked for.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{header, Method, StatusCode};
use serde::Serialize;

use super::{build, method_not_allowed, ops_error, Ctx, Handled};
use crate::error::{ApiError, ErrorCode};
use crate::reference::{parse_digest, valid_tag, Reference};
use crate::seam::{ManifestInfo, RepoDetail, RepoSummary, TagEventInfo, TagInfo, Tally};

/// Rows per page when `?n=` is absent.
pub const DEFAULT_PAGE: usize = 25;
/// Ceiling for `?n=`. Clamped rather than rejected, as everywhere else.
pub const MAX_PAGE: usize = 100;

/// One `/api/v1/` operation, with the repository name already split out.
///
/// Each collection is its own top-level resource rather than a path nested
/// under the repository, because a repository name may contain `/` and a nested
/// listing would then be ambiguous. See [`crate::app::api_route`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiEndpoint {
    /// `GET /api/v1/repositories`
    Repositories,
    /// `GET /api/v1/repositories/<name>`
    Repository { name: String },
    /// `GET /api/v1/tags/<name>`
    Tags { name: String },
    /// `GET /api/v1/manifests/<name>`
    Manifests { name: String },
    /// `GET /api/v1/manifests/<name>@<reference>`
    Manifest { name: String, reference: String },
    /// `GET /api/v1/tag-history/<name>@<reference>`, where `<reference>` is a
    /// tag or a digest and the two ask different questions - see
    /// [`crate::seam::Registry::tag_history`].
    TagHistory { name: String, reference: String },
}

// ---- wire shapes ---------------------------------------------------------

#[derive(Serialize)]
struct TallyBody {
    count: u64,
    /// `false` means `count` is a floor: the scan stopped at its ceiling.
    complete: bool,
}

impl From<Tally> for TallyBody {
    fn from(tally: Tally) -> Self {
        TallyBody {
            count: tally.count,
            complete: tally.complete,
        }
    }
}

#[derive(Serialize)]
struct RepoRow {
    name: String,
    tags: TallyBody,
    manifests: TallyBody,
}

#[derive(Serialize)]
struct RepositoriesBody {
    repositories: Vec<RepoRow>,
    /// Pass back as `?last=`. `null` when the listing is exhausted - decided by
    /// peeking one key past the page, never by "the page came back full".
    next: Option<String>,
}

#[derive(Serialize)]
struct RepositoryBody {
    name: String,
    tags: TallyBody,
    manifests: TallyBody,
    blobs: TallyBody,
    /// Summed over the blobs counted, so a floor whenever `blobs.complete` is
    /// `false`.
    size_bytes: u64,
}

#[derive(Serialize)]
struct ManifestBody {
    digest: String,
    media_type: String,
    size: u64,
    blob_size: u64,
    artifact_type: Option<String>,
    subject: Option<String>,
    pushed_at: u64,
    platforms: Vec<String>,
    blobs: u64,
    children: u64,
    tags: Vec<String>,
    annotations: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct TagBody {
    name: String,
    digest: String,
    tagged_at: u64,
    manifest: Option<ManifestBody>,
}

#[derive(Serialize)]
struct TagsBody {
    tags: Vec<TagBody>,
    next: Option<String>,
}

#[derive(Serialize)]
struct ManifestsBody {
    manifests: Vec<ManifestBody>,
    /// A digest, to pass back as `?last=`.
    next: Option<String>,
}

/// One tag event.
///
/// `at` is unix **milliseconds**, unlike `tagged_at` and `pushed_at` next door,
/// which are seconds. Tag events are ordered by this value and a second is not
/// fine enough: two events on one tag inside a second collide.
#[derive(Serialize)]
struct TagEventBody {
    at: u64,
    tag: String,
    digest: String,
    event: &'static str,
    /// The manifest's media type and size *at the time of the event*, kept in
    /// the event itself so a row still renders after the manifest is gone.
    media_type: String,
    size: u64,
}

/// Both halves of the resume position, because events can share an instant.
#[derive(Serialize)]
struct HistoryCursorBody {
    before: u64,
    last: String,
}

#[derive(Serialize)]
struct TagHistoryBody {
    events: Vec<TagEventBody>,
    next: Option<HistoryCursorBody>,
}

impl From<TagEventInfo> for TagEventBody {
    fn from(event: TagEventInfo) -> Self {
        TagEventBody {
            at: event.at,
            tag: event.tag,
            digest: event.digest.to_string(),
            // The vocabulary distribution-spec#606 proposes for the same two
            // events, so a future `_oci/tag-history` response is a rename of
            // the envelope rather than of the rows.
            event: if event.deleted { "deleted" } else { "created" },
            media_type: event.media_type,
            size: event.size,
        }
    }
}

impl From<ManifestInfo> for ManifestBody {
    fn from(info: ManifestInfo) -> Self {
        ManifestBody {
            digest: info.digest.to_string(),
            media_type: info.media_type,
            size: info.size,
            blob_size: info.blob_size,
            artifact_type: info.artifact_type,
            subject: info.subject.map(|d| d.to_string()),
            pushed_at: info.pushed_at,
            platforms: info.platforms,
            blobs: info.blobs,
            children: info.children,
            tags: info.tags,
            annotations: info.annotations,
        }
    }
}

impl From<RepoSummary> for RepoRow {
    fn from(summary: RepoSummary) -> Self {
        RepoRow {
            name: summary.name,
            tags: summary.tags.into(),
            manifests: summary.manifests.into(),
        }
    }
}

impl From<RepoDetail> for RepositoryBody {
    fn from(detail: RepoDetail) -> Self {
        RepositoryBody {
            name: detail.name,
            tags: detail.tags.into(),
            manifests: detail.manifests.into(),
            blobs: detail.blobs.into(),
            size_bytes: detail.size_bytes,
        }
    }
}

impl From<TagInfo> for TagBody {
    fn from(info: TagInfo) -> Self {
        TagBody {
            name: info.name,
            digest: info.digest.to_string(),
            tagged_at: info.tagged_at,
            manifest: info.manifest.map(ManifestBody::from),
        }
    }
}

// ---- dispatch ------------------------------------------------------------

pub async fn handle(ctx: &Ctx, endpoint: ApiEndpoint) -> Handled {
    // Read-only, deliberately. Everything that changes the registry is a `/v2/`
    // operation with a spec-defined meaning, and a second way to do it would be
    // a second set of rules to keep in agreement with the first.
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        return Err(method_not_allowed("GET, HEAD"));
    }

    match endpoint {
        ApiEndpoint::Repositories => repositories(ctx).await,
        ApiEndpoint::Repository { name } => repository(ctx, &name).await,
        ApiEndpoint::Tags { name } => tags(ctx, &name).await,
        ApiEndpoint::Manifests { name } => manifests(ctx, &name).await,
        ApiEndpoint::Manifest { name, reference } => manifest(ctx, &name, &reference).await,
        ApiEndpoint::TagHistory { name, reference } => tag_history(ctx, &name, &reference).await,
    }
}

async fn repositories(ctx: &Ctx) -> Handled {
    let (limit, last) = page_params(ctx)?;
    // `?q=` is a name *prefix*, not a substring: it narrows the key scan, so it
    // costs a seek rather than a pass over the catalogue. That is the whole
    // reason search can exist at ten million repositories at all, and it is
    // worth the UI having to say "starts with" rather than "contains".
    let prefix = ctx.param("q").unwrap_or_default();
    let page = ctx
        .registry()
        .repository_summaries(prefix, last.as_deref(), limit)
        .await
        .map_err(ops_error)?;

    // The cursor is the last row's own key, so it is only meaningful if there
    // is a row *and* something after it.
    let next = page
        .more
        .then(|| page.items.last().map(|row| row.name.clone()))
        .flatten();
    json(
        ctx,
        &RepositoriesBody {
            repositories: page.items.into_iter().map(RepoRow::from).collect(),
            next,
        },
    )
}

async fn repository(ctx: &Ctx, name: &str) -> Handled {
    let detail = ctx
        .registry()
        .repository_detail(name)
        .await
        .map_err(ops_error)?;
    json(ctx, &RepositoryBody::from(detail))
}

async fn tags(ctx: &Ctx, name: &str) -> Handled {
    let (limit, last) = page_params(ctx)?;
    let page = ctx
        .registry()
        .tag_details(name, last.as_deref(), limit)
        .await
        .map_err(ops_error)?;

    let next = page
        .more
        .then(|| page.items.last().map(|tag| tag.name.clone()))
        .flatten();
    json(
        ctx,
        &TagsBody {
            tags: page.items.into_iter().map(TagBody::from).collect(),
            next,
        },
    )
}

async fn manifests(ctx: &Ctx, name: &str) -> Handled {
    let (limit, last) = page_params(ctx)?;
    // The manifest range is digest-ordered, so its cursor is a digest and it is
    // validated under the same grammar a `/v2/` path segment gets. A cursor
    // that does not parse is a `400`, not a silent restart from the top.
    let last = match last.as_deref() {
        Some(raw) => Some(parse_digest(raw).map_err(|e| {
            ApiError::new(ErrorCode::DigestInvalid)
                .with_message("invalid cursor")
                .with_detail(e.to_string())
        })?),
        None => None,
    };
    let page = ctx
        .registry()
        .manifest_details(name, last.as_ref(), limit)
        .await
        .map_err(ops_error)?;

    let next = page
        .more
        .then(|| page.items.last().map(|m| m.digest.to_string()))
        .flatten();
    json(
        ctx,
        &ManifestsBody {
            manifests: page.items.into_iter().map(ManifestBody::from).collect(),
            next,
        },
    )
}

async fn manifest(ctx: &Ctx, name: &str, reference: &str) -> Handled {
    let reference = parse_reference(reference)?;
    let info = ctx
        .registry()
        .manifest_detail(name, &reference)
        .await
        .map_err(ops_error)?;
    json(ctx, &ManifestBody::from(info))
}

/// Tag history, newest first.
///
/// `?before=` is a filter in its own right - "what did this look like last
/// Tuesday" - and `?before=` plus `?last=` is the exact resume a `next` cursor
/// hands back. Both are needed because two events can share a millisecond, so a
/// page can end inside one instant and an instant-only cursor would skip the
/// rest of it.
async fn tag_history(ctx: &Ctx, name: &str, reference: &str) -> Handled {
    let (limit, last) = page_params(ctx)?;
    let reference = parse_reference(reference)?;
    let before = match ctx.param("before") {
        Some(raw) => Some(raw.parse::<u64>().map_err(|_| {
            ApiError::new(ErrorCode::PaginationNumberInvalid)
                .with_message("invalid before")
                .with_detail(format!("before={raw}"))
        })?),
        None => None,
    };

    let (events, next) = ctx
        .registry()
        .tag_history(name, &reference, before, last.as_deref(), limit)
        .await
        .map_err(ops_error)?;

    json(
        ctx,
        &TagHistoryBody {
            events: events.into_iter().map(TagEventBody::from).collect(),
            next: next.map(|c| HistoryCursorBody {
                before: c.before,
                last: c.last,
            }),
        },
    )
}

// ---- helpers -------------------------------------------------------------

/// `?n=` and `?last=`, with this API's own bounds.
///
/// Deliberately not [`crate::pagination::parse`]: that one carries the spec's
/// `?n=0` rule and the operator's `/v2/` page limits, which are sized for an
/// endpoint that costs one seek. A row here costs a bounded count per
/// repository, so it needs a much smaller ceiling and has no reason to
/// reproduce the `n=0` special case.
fn page_params(ctx: &Ctx) -> Result<(usize, Option<String>), ApiError> {
    let limit = match ctx.param("n") {
        None => DEFAULT_PAGE,
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| {
                ApiError::new(ErrorCode::PaginationNumberInvalid).with_detail(format!("n={raw}"))
            })?
            .clamp(1, MAX_PAGE),
    };
    let last = ctx
        .param("last")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Ok((limit, last))
}

/// A tag or a digest, under the same rule `/v2/` uses: a `:` means digest.
fn parse_reference(raw: &str) -> Result<Reference, ApiError> {
    if raw.contains(':') {
        return parse_digest(raw)
            .map(Reference::Digest)
            .map_err(|e| ApiError::new(ErrorCode::DigestInvalid).with_detail(e.to_string()));
    }
    if !valid_tag(raw) {
        return Err(ApiError::new(ErrorCode::ManifestUnknown).with_detail(raw.to_owned()));
    }
    Ok(Reference::Tag(raw.to_owned()))
}

/// Serialise, and honour `HEAD` by keeping the `Content-Length` the body would
/// have had.
fn json<T: Serialize>(ctx: &Ctx, body: &T) -> Handled {
    let bytes = serde_json::to_vec(body).map_err(|e| ApiError::internal(e.to_string()))?;
    let builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, bytes.len())
        // A discovery answer is true for as long as it takes the next push to
        // land, which is to say not long. Caching it would make the UI show a
        // registry that no longer exists.
        .header(header::CACHE_CONTROL, "no-store");

    if ctx.method == Method::HEAD {
        return Ok(build(builder, Body::empty()));
    }
    Ok(build(builder, Body::from(bytes)))
}
