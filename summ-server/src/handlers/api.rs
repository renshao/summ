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

use super::{build, empty_with_length, method_not_allowed, ops_error, Ctx, Handled};
use crate::error::{ApiError, ErrorCode};
use crate::reference::{parse_digest, valid_tag, Reference};
use crate::seam::{
    ManifestInfo, PullCountDay, PullCountScope, RepoDetail, RepoSummary, TagEventInfo, TagInfo,
    Tally,
};

/// Rows per page when `?n=` is absent.
pub const DEFAULT_PAGE: usize = 25;
/// Ceiling for `?n=`. Clamped rather than rejected, as everywhere else.
pub const MAX_PAGE: usize = 100;

/// Days of pull counts when `?days=` is absent - what the UI's daily grid
/// shows.
pub const DEFAULT_PULL_COUNT_DAYS: u16 = 30;
/// Ceiling for `?days=`. Fifty-three weeks is 371, so this is a year of wall
/// plus slack, and it is what bounds the scan: there is no cursor here because
/// the window is the limit.
pub const MAX_PULL_COUNT_DAYS: u16 = 400;

/// One `/api/v1/` operation, with the repository name already split out.
///
/// Each collection is its own top-level resource rather than a path nested
/// under the repository, because a repository name may contain `/` and a nested
/// listing would then be ambiguous. See [`crate::app::api_route`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiEndpoint {
    /// `GET /api/v1/repositories`
    Repositories,
    /// `GET /api/v1/repositories/<name>`, and `DELETE` of the same - the one
    /// mutating route on this API.
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
    /// `GET /api/v1/pull-counts/<name>` or `.../<name>@<reference>`.
    ///
    /// The reference is optional because the repository is itself a scope, and
    /// the only one carrying blob traffic. A tag and a digest ask different
    /// questions again - "how often is this name pulled" against "how often is
    /// this content pulled" - and they are separate series, not views of one.
    PullCounts {
        name: String,
        reference: Option<String>,
    },
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

/// One day of counters, zero-filled.
///
/// Every day in the window is present whether or not it saw traffic, because
/// the client is a grid and a gap in a grid is a missing cell rather than a
/// zero one. The store returns only days with counts; filling them in is this
/// layer's job precisely because it is the layer that knows the shape.
#[derive(Serialize)]
struct PullCountDayBody {
    /// Days since the Unix epoch, UTC - the stored bucket, unmodified.
    day: u16,
    /// The same day as `YYYY-MM-DD`, so a client does not have to agree with
    /// the server about calendars to label a column.
    date: String,
    /// `0` is Sunday. A contribution grid's row index, and nothing is stored
    /// for it: 1970-01-01 was a Thursday, so it is `(day + 4) % 7`.
    weekday: u8,
    /// The day, which is the sum of `hours`. There is no stored total.
    manifest_pulls: u64,
    blob_pulls: u64,
    bytes_out: u64,
    hours: PullCountHoursBody,
}

/// The day broken down by hour, UTC, index `0..24`.
///
/// This is what makes the numbers honest outside UTC. The day bucket is fixed
/// at write time and must never be re-bucketed - the same wall would change
/// shape depending on who was looking at it - but hours can be re-summed into
/// any zone, and they answer "when in the day does this get pulled" from the
/// same response.
#[derive(Serialize)]
struct PullCountHoursBody {
    manifest_pulls: Vec<u32>,
    blob_pulls: Vec<u32>,
    bytes_out: Vec<u64>,
}

#[derive(Serialize)]
struct PullCountTotalsBody {
    manifest_pulls: u64,
    blob_pulls: u64,
    bytes_out: u64,
}

#[derive(Serialize)]
struct PullCountsBody {
    repository: String,
    /// Absent for the repository scope.
    reference: Option<String>,
    /// `repository`, `tag` or `manifest`.
    scope: &'static str,
    /// **Always `true`.** Increments are held in memory between flushes, so a
    /// crash loses up to one interval and a saturated accumulator drops the
    /// tail of a spike. A field that never varies is still worth sending: it is
    /// how a client learns these are a popularity signal and not billing data,
    /// and the alternative is that nothing says so anywhere the client looks.
    approximate: bool,
    /// The window, inclusive at both ends.
    from: String,
    to: String,
    totals: PullCountTotalsBody,
    days: Vec<PullCountDayBody>,
}

// ---- dispatch ------------------------------------------------------------

pub async fn handle(ctx: &Ctx, endpoint: ApiEndpoint) -> Handled {
    // Read-only but for one route. Everything with a spec-defined meaning is a
    // `/v2/` operation and stays there, because a second way to do it would be
    // a second set of rules to keep in agreement with the first. Deleting a
    // repository has no spec spelling at all - `/v2/` cannot express it - so it
    // lives here, on the resource it deletes.
    if ctx.method == Method::DELETE {
        return match endpoint {
            ApiEndpoint::Repository { name } => delete_repository(ctx, &name).await,
            _ => Err(method_not_allowed("GET, HEAD")),
        };
    }
    if ctx.method != Method::GET && ctx.method != Method::HEAD {
        return Err(method_not_allowed(match endpoint {
            ApiEndpoint::Repository { .. } => "GET, HEAD, DELETE",
            _ => "GET, HEAD",
        }));
    }

    match endpoint {
        ApiEndpoint::Repositories => repositories(ctx).await,
        ApiEndpoint::Repository { name } => repository(ctx, &name).await,
        ApiEndpoint::Tags { name } => tags(ctx, &name).await,
        ApiEndpoint::Manifests { name } => manifests(ctx, &name).await,
        ApiEndpoint::Manifest { name, reference } => manifest(ctx, &name, &reference).await,
        ApiEndpoint::TagHistory { name, reference } => tag_history(ctx, &name, &reference).await,
        ApiEndpoint::PullCounts { name, reference } => {
            pull_counts(ctx, &name, reference.as_deref()).await
        }
    }
}

async fn repositories(ctx: &Ctx) -> Handled {
    let (limit, last) = page_params(ctx)?;
    // `?q=` matches anywhere in the name. A prefix would ride the key order and
    // cost a seek; a substring cannot, so this walks the name range instead -
    // cheap keys, no value decodes, and a bound on how far one request goes.
    // The bound is why the cursor comes from the scan rather than from the last
    // row here: a page may skip rows, or stop on a name it never served.
    // Lowercased because a repository name cannot hold an uppercase byte - see
    // `reference::valid_name` - so an uppercase needle is not a search that
    // finds nothing, it is a search that *cannot* find anything. Folding it
    // here rather than in the ops layer keeps the match itself literal: this is
    // a fact about what a person typed, not about how names are stored.
    let query = ctx.param("q").unwrap_or_default().to_ascii_lowercase();
    let page = ctx
        .registry()
        .repository_summaries(&query, last.as_deref(), limit)
        .await
        .map_err(ops_error)?;

    json(
        ctx,
        &RepositoriesBody {
            repositories: page.items.into_iter().map(RepoRow::from).collect(),
            next: page.next,
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

/// `DELETE /api/v1/repositories/<name>`.
///
/// `202 Accepted`, and it means what it says here rather than what it means on
/// a manifest delete: the repository is gone from every listing by the time
/// this returns, but the keys underneath it may still be being swept. There is
/// nothing a client can observe that distinguishes the two, which is the
/// point: a `GET` of the repository, its tags or its manifests answers `404`
/// immediately.
///
/// Blob bytes are not reclaimed. Layers are shared registry-wide and whether
/// this repository was the last user of one is purge's question; a client that
/// deletes a repository to free disk gets the space when purge runs.
async fn delete_repository(ctx: &Ctx, name: &str) -> Handled {
    ctx.registry()
        .delete_repository(name)
        .await
        .map_err(ops_error)?;
    Ok(empty_with_length(StatusCode::ACCEPTED, 0, Vec::new()))
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

/// Pull counts over a day window, zero-filled, hour by hour.
///
/// There is no cursor and no `next`: the window *is* the bound, 53 weeks is 371
/// keys, and the whole visualisation is one ordered scan. `?days=` moves the
/// window's length; it always ends today, because a wall that does not end now
/// is a wall nobody asked for.
///
/// Nothing here 404s. An unknown repository, tag or manifest is a window of
/// zeroes - counts outlive what they describe, and after a delete nothing
/// distinguishes "never pulled" from "gone", but the second case must still
/// answer.
async fn pull_counts(ctx: &Ctx, name: &str, reference: Option<&str>) -> Handled {
    // Parsed wide and then clamped, not parsed as the target width: `?days=`
    // is clamped rather than rejected like every other bound in this API, and
    // parsing straight into a `u16` would turn an oversized-but-well-formed
    // number into a `400` while a merely large one succeeded. Only something
    // that is not a number at all is an error.
    let days = match ctx.param("days") {
        None => DEFAULT_PULL_COUNT_DAYS,
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|_| {
                ApiError::new(ErrorCode::PaginationNumberInvalid)
                    .with_message("invalid days")
                    .with_detail(format!("days={raw}"))
            })?
            .clamp(1, MAX_PULL_COUNT_DAYS as u64) as u16,
    };

    let scope = match reference {
        None => PullCountScope::Repository,
        Some(raw) => match parse_reference(raw)? {
            Reference::Tag(tag) => PullCountScope::Tag(tag),
            Reference::Digest(digest) => PullCountScope::Manifest(digest),
        },
    };

    // The window ends on today's bucket, in UTC, which is the same boundary the
    // counters were written against. Deriving it from a viewer's zone here
    // would silently shift every column by a day for half the world.
    let today = summ_core::keys::day_bucket(unix_now());
    let from = today.saturating_sub(days.saturating_sub(1));

    let series = ctx
        .registry()
        .pull_counts(name, &scope, from, days)
        .await
        .map_err(ops_error)?;

    json(
        ctx,
        &pull_counts_body(name, reference, &scope, from, days, series),
    )
}

fn pull_counts_body(
    name: &str,
    reference: Option<&str>,
    scope: &PullCountScope,
    from: u16,
    days: u16,
    series: Vec<PullCountDay>,
) -> PullCountsBody {
    // The store returns only days with traffic, in order, so filling the window
    // is a merge rather than a map: walk the window and take the next stored day
    // whenever it matches.
    let mut stored = series.into_iter().peekable();
    let mut rows = Vec::with_capacity(days as usize);
    let mut totals = PullCountTotalsBody {
        manifest_pulls: 0,
        blob_pulls: 0,
        bytes_out: 0,
    };

    for offset in 0..days {
        let day = from.saturating_add(offset);
        let bucket = match stored.peek() {
            Some(next) if next.day == day => stored.next().map(|d| d.bucket).unwrap_or_default(),
            _ => summ_core::CounterBucket::default(),
        };
        totals.manifest_pulls += bucket.manifest_pulls_total();
        totals.blob_pulls += bucket.blob_pulls_total();
        totals.bytes_out += bucket.bytes_out_total();
        rows.push(PullCountDayBody {
            day,
            date: iso_date(day),
            weekday: summ_core::keys::weekday(day),
            manifest_pulls: bucket.manifest_pulls_total(),
            blob_pulls: bucket.blob_pulls_total(),
            bytes_out: bucket.bytes_out_total(),
            hours: PullCountHoursBody {
                manifest_pulls: bucket.manifest_pulls.to_vec(),
                blob_pulls: bucket.blob_pulls.to_vec(),
                bytes_out: bucket.bytes_out.to_vec(),
            },
        });
    }

    PullCountsBody {
        repository: name.to_string(),
        reference: reference.map(str::to_owned),
        scope: scope.label(),
        approximate: true,
        from: iso_date(from),
        to: iso_date(from.saturating_add(days.saturating_sub(1))),
        totals,
        days: rows,
    }
}

// ---- helpers -------------------------------------------------------------

/// Seconds since the Unix epoch, or the epoch itself if the clock is before it.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD` for a day bucket, UTC.
///
/// Hinnant's civil-from-days, which is exact over the whole `u16` range and
/// wants no dependency. The alternative was to send the bucket number alone and
/// let each client do this; the bucket is sent too, but a label a UI can print
/// without a calendar library belongs in the response.
fn iso_date(day: u16) -> String {
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

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

#[cfg(test)]
mod tests {
    use super::iso_date;

    /// The one thing `iso_date` can get wrong is a leap year, and it gets it
    /// wrong silently: every column of a wall shifts by a day.
    #[test]
    fn day_buckets_become_the_dates_they_name() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1), "1970-01-02");
        // 2000 is a leap year, 1900 was not - the rule the naive version misses.
        assert_eq!(iso_date(11_016), "2000-02-29");
        assert_eq!(iso_date(11_017), "2000-03-01");
        assert_eq!(iso_date(19_723), "2024-01-01");
        assert_eq!(iso_date(19_782), "2024-02-29");
        // The far end of the u16 range still decodes rather than wrapping.
        assert_eq!(iso_date(u16::MAX), "2149-06-06");
    }
}
