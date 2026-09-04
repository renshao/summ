//! Tag history - the read side of the `H` and `J` ranges.
//!
//! Nothing here writes. The events have been written on every tag mutation
//! since Phase 1, in the same [`WriteBatch`](summ_meta::WriteBatch) as the
//! mutation itself, by [`Registry::stage_set_tag`] and
//! [`Registry::stage_delete_tag`]; this module is what finally reads them back.
//!
//! Four properties of the storage that a caller has to know about, because they
//! decide what the answers mean:
//!
//! - **Newest first, and free.** The timestamp is stored complemented, so a
//!   forward scan - the only kind [`MetaEngine`](summ_meta::MetaEngine) has -
//!   arrives in descending order with nothing to sort.
//! - **The cursor is real values, not a token.** `before` alone is a filter -
//!   strictly-before that instant - and a caller can ask for a window with it
//!   directly. Paging adds `last`, the tiebreaker off the final row, because
//!   two events can share a millisecond and a page can end inside one; see
//!   [`HistoryCursor`].
//! - **It is an event log, not a list of what the tag pointed to.** A repoint
//!   writes one `Created` for the new digest and no `Deleted` for the displaced
//!   one, so "what did this tag point to at time T" is the newest `Created` at
//!   or before T, unless a `Deleted` is newer. And because the event is written
//!   unconditionally, pushing the same tag at the same digest twice is two
//!   events - it records pushes, not changes.
//! - **It outlives the manifest.** `media_type` and `size` are denormalised into
//!   the event precisely so a row still renders after `M <repo> <digest>` is
//!   gone, which is the state every `Deleted` event describes.
//!
//! Neither range is bounded by current state - they are the only ranges that
//! grow with time rather than with content - and nothing trims them yet. A
//! retention window is a `DeletePrefix` over a bounded suffix of each range and
//! belongs to the purge sweep; see `PLAN.md`.

use summ_core::{keys, Digest, TagEvent, Timestamp};

use crate::codec::decode;
use crate::error::{RegistryError, Result};
use crate::registry::Registry;

/// One row of history: the key's timestamp joined to the value's event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEventEntry {
    pub at: Timestamp,
    pub tag: String,
    pub digest: Digest,
    pub event: TagEvent,
}

/// Where the next page resumes.
///
/// A timestamp alone is not enough, and that is not a theoretical worry: the
/// key's tiebreaker is the digest (for `H`) or the tag (for `J`), so two events
/// can share a millisecond, and a page can end in the middle of that instant.
/// Resuming from the instant alone would then skip its remaining events - the
/// exact silent hole in an audit trail that storing milliseconds rather than
/// seconds exists to prevent.
///
/// Both halves are values the caller already has, so this stays the house
/// pagination style - a real value off the last row, not an opaque token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryCursor {
    pub before: Timestamp,
    /// The digest of the last `H` row, the tag of the last `J` row.
    pub last: String,
}

/// A page of history, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagHistory {
    pub events: Vec<TagEventEntry>,
    /// `None` means the scan is exhausted.
    pub next: Option<HistoryCursor>,
}

impl Registry {
    /// `H <repo> <tag>` - one tag's history, newest first.
    ///
    /// `before` on its own is the spec's filter, and is strictly-before.
    /// Together with `last` it is an exact resume, which is what a `Link`
    /// emits. An unknown repo or tag is an empty page rather than an error:
    /// history outlives the tag it describes, so a deleted tag must still
    /// answer, and there is nothing left to tell the two cases apart.
    pub fn tag_history(
        &self,
        repo: &str,
        tag: &str,
        before: Option<Timestamp>,
        last: Option<&str>,
        limit: usize,
    ) -> Result<TagHistory> {
        let Some(repo_id) = self.lookup_repo(repo)? else {
            return Ok(TagHistory::default());
        };
        let prefix = keys::tag_history_of(repo_id, tag);
        let cursor = match (before, last) {
            (Some(at), Some(raw)) => Some(keys::tag_history(repo_id, tag, at, &parse_digest(raw)?)),
            (Some(at), None) => Some(keys::tag_history_before(repo_id, tag, at)),
            (None, _) => None,
        };
        let page = self.engine().scan(&prefix, cursor.as_deref(), limit)?;

        let mut events = Vec::with_capacity(page.entries.len());
        for (key, value) in &page.entries {
            let (at, digest) = keys::tag_history_suffix(key, &prefix)
                .ok_or_else(|| RegistryError::corrupt("tag history key"))?;
            events.push(TagEventEntry {
                at,
                tag: tag.to_string(),
                digest,
                event: decode(value, "TagEvent")?,
            });
        }
        Ok(TagHistory {
            next: next_cursor(&events, page.next.is_some(), |e| e.digest.to_string()),
            events,
        })
    }

    /// `J <repo> <digest>` - what this manifest was ever tagged, and when.
    pub fn manifest_tag_history(
        &self,
        repo: &str,
        digest: &Digest,
        before: Option<Timestamp>,
        last: Option<&str>,
        limit: usize,
    ) -> Result<TagHistory> {
        let Some(repo_id) = self.lookup_repo(repo)? else {
            return Ok(TagHistory::default());
        };
        let prefix = keys::manifest_tag_history_of(repo_id, digest);
        let cursor = match (before, last) {
            (Some(at), Some(tag)) => Some(keys::manifest_tag_history(repo_id, digest, at, tag)),
            (Some(at), None) => Some(keys::manifest_tag_history_before(repo_id, digest, at)),
            (None, _) => None,
        };
        let page = self.engine().scan(&prefix, cursor.as_deref(), limit)?;

        let mut events = Vec::with_capacity(page.entries.len());
        for (key, value) in &page.entries {
            let (at, tag) = keys::manifest_tag_history_suffix(key, &prefix)
                .ok_or_else(|| RegistryError::corrupt("manifest tag history key"))?;
            events.push(TagEventEntry {
                at,
                tag,
                digest: *digest,
                event: decode(value, "TagEvent")?,
            });
        }
        Ok(TagHistory {
            next: next_cursor(&events, page.next.is_some(), |e| e.tag.clone()),
            events,
        })
    }
}

fn parse_digest(raw: &str) -> Result<Digest> {
    raw.parse().map_err(|_| RegistryError::DigestInvalid {
        reason: format!("{raw} is not a digest"),
    })
}

fn next_cursor(
    events: &[TagEventEntry],
    more: bool,
    last: impl Fn(&TagEventEntry) -> String,
) -> Option<HistoryCursor> {
    if !more {
        return None;
    }
    events.last().map(|e| HistoryCursor {
        before: e.at,
        last: last(e),
    })
}
