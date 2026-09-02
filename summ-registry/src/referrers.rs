//! The referrers query.
//!
//! `GET /v2/<name>/referrers/<digest>` returns an image index whose entries
//! require `artifactType` and `annotations`, and whose `?artifactType=` filter
//! tests them. That is why the `F` edge carries a [`ReferrerRecord`] instead of
//! being valueless: with the descriptor on the edge the whole endpoint is one
//! ordered prefix scan with the filter applied *during* it, where a valueless
//! edge would need a point lookup and a decode per referrer before it could
//! even evaluate the filter.
//!
//! Nothing routes here yet - `/referrers/` stays 404 until Phase 6 - but the
//! edges are written from Phase 1, because retrofitting them costs a full
//! manifest rescan plus a spec-mandated ingest of the fallback tag schema.

use summ_core::{keys, Digest, ReferrerRecord};

use crate::codec::decode;
use crate::error::{RegistryError, Result};
use crate::registry::Registry;
use crate::suffix;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferrerEntry {
    pub digest: Digest,
    pub record: ReferrerRecord,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferrerList {
    pub entries: Vec<ReferrerEntry>,
    /// Referrer digest to resume after. Note this advances over the *scanned*
    /// range, not over the entries returned, so a filtered page can come back
    /// short - even empty - with `next` still set.
    pub next: Option<Digest>,
    /// Whether the `artifactType` filter was applied, which the response must
    /// declare through `OCI-Filters-Applied`. Claiming the filter without
    /// applying it exactly is worse than not claiming it: the suite then checks
    /// that no descriptor of another type is present.
    pub filter_applied: bool,
}

impl Registry {
    /// Referrers of one subject, in digest order, optionally filtered.
    ///
    /// A missing repository or an unknown subject is an empty list, never an
    /// error the caller should turn into a 404: "if the registry supports the
    /// referrers API, the registry MUST NOT return a 404 Not Found to a
    /// referrers API request".
    ///
    /// The filter is applied inside the scan and the cursor advances over the
    /// raw edge range. That means a page may hold fewer entries than `limit`
    /// while more matches exist further on, and the caller pages until `next`
    /// is `None`. The alternative - refilling until the page is full - makes a
    /// single request scan an unbounded number of edges whenever the requested
    /// artifact type is rare, which is exactly the shape this design forbids.
    pub fn referrers(
        &self,
        repo: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<ReferrerList> {
        let Some(repo_id) = self.lookup_repo(repo)? else {
            return Ok(ReferrerList {
                filter_applied: artifact_type.is_some(),
                ..ReferrerList::default()
            });
        };

        let prefix = keys::referrers_of(repo_id, subject);
        let cursor = start_after.map(|d| keys::referrer(repo_id, subject, d));
        let page = self.engine().scan(&prefix, cursor.as_deref(), limit)?;

        let mut entries = Vec::with_capacity(page.entries.len());
        for (key, value) in &page.entries {
            let digest = suffix::second_digest_after_repo(key)
                .ok_or_else(|| RegistryError::corrupt("referrer key"))?;
            let record: ReferrerRecord = decode(value, "ReferrerRecord")?;
            if let Some(wanted) = artifact_type {
                if record.artifact_type.as_deref() != Some(wanted) {
                    continue;
                }
            }
            entries.push(ReferrerEntry { digest, record });
        }

        let next = match page.next.as_deref() {
            Some(key) => Some(
                suffix::second_digest_after_repo(key)
                    .ok_or_else(|| RegistryError::corrupt("referrer cursor"))?,
            ),
            None => None,
        };

        Ok(ReferrerList {
            entries,
            next,
            filter_applied: artifact_type.is_some(),
        })
    }
}
