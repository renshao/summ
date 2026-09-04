//! In-progress blob uploads - the `U <uuid>` key range.
//!
//! An upload session is metadata, not storage. The staged bytes belong to the
//! blob store; what lives here is the pair that makes the upload resumable -
//! the committed `offset` and the hasher state at exactly that offset - plus
//! the repo the session belongs to and the algorithm it was opened under.
//!
//! Keeping the session here rather than beside the staging file is deliberate,
//! and it is the one place summ improves on distribution's arrangement rather
//! than merely differing from it. distribution writes `_uploads/<id>/startedat`
//! and a `hashstates/<algo>/<offset>` file per resume point, so an interrupted
//! upload can only continue on the node that holds those files. Here the resume
//! point is 104 bytes inside a record that is already transactional with
//! everything else, so a chunked upload can continue on any process - which is
//! what stops chunked uploads from becoming an HA constraint.
//!
//! Two invariants the callers depend on:
//!
//! - **`offset` and `hasher_state` are written in the same batch, always.**
//!   Either alone is useless, and a pair from different moments resumes onto a
//!   hole. [`Registry::plan_save_upload`] takes the whole record for that
//!   reason rather than offering field-wise updates.
//! - **The session record is committed after the bytes it describes are
//!   written.** Same ordering rule as a blob: staged bytes ahead of the
//!   recorded offset are garbage the resume truncates away, whereas a recorded
//!   offset ahead of the bytes is corruption.

use summ_core::{keys, RepoId, Timestamp, UploadSession};
use summ_meta::WriteBatch;

use crate::codec::{decode, encode};
use crate::error::{RegistryError, Result};
use crate::registry::{Planned, Registry};

/// The 16 raw bytes of the upload's UUID, which is what `U <uuid>` keys on.
///
/// The HTTP layer mints the id and carries it as text; the key range holds the
/// bytes. Neither form is derived from the other here - the caller converts
/// once, at the edge, because an id that does not parse is a malformed request
/// rather than a storage failure.
pub type UploadKey = [u8; 16];

impl Registry {
    /// Open a session. The repo is interned, because a `U` record names it by
    /// id and an upload may well be the first write a repository ever sees.
    pub fn create_upload(
        &self,
        repo: &str,
        id: &UploadKey,
        algorithm: &str,
        now: Timestamp,
    ) -> Result<UploadSession> {
        let planned = self.plan_create_upload(repo, id, algorithm, now)?;
        self.engine().apply(&planned.batch)?;
        Ok(planned.outcome)
    }

    pub fn plan_create_upload(
        &self,
        repo: &str,
        id: &UploadKey,
        algorithm: &str,
        now: Timestamp,
    ) -> Result<Planned<UploadSession>> {
        let repo_id = self.intern_repo(repo)?;
        let session = UploadSession {
            repo: repo_id,
            offset: 0,
            started_at: now.secs(),
            updated_at: now.secs(),
            algorithm: algorithm.to_string(),
            // No bytes have been hashed, so there is no state worth carrying:
            // a resume at offset 0 starts a fresh hasher either way.
            hasher_state: None,
        };
        let mut batch = WriteBatch::new();
        batch.put(keys::upload(id), encode(&session)?);
        Ok(Planned {
            outcome: session,
            batch,
        })
    }

    pub fn get_upload(&self, id: &UploadKey) -> Result<Option<UploadSession>> {
        match self.engine().get(&keys::upload(id))? {
            Some(raw) => Ok(Some(decode(&raw, "UploadSession")?)),
            None => Ok(None),
        }
    }

    /// The session, but only if it belongs to `repo`.
    ///
    /// The upload id alone would be enough to find the record. Checking the
    /// name as well is what stops one repository from continuing - or
    /// cancelling - another's upload by guessing a `Location`, and it costs one
    /// interner lookup that the surrounding request has usually done already.
    /// A mismatch is `None`, never a distinguishable error: a client that
    /// cannot see the session must not be able to learn that it exists.
    pub fn get_upload_in(&self, repo: &str, id: &UploadKey) -> Result<Option<UploadSession>> {
        let Some(session) = self.get_upload(id)? else {
            return Ok(None);
        };
        match self.lookup_repo(repo)? {
            Some(repo_id) if repo_id == session.repo => Ok(Some(session)),
            _ => Ok(None),
        }
    }

    /// Persist an advanced session. The caller supplies the whole record so
    /// `offset` and `hasher_state` can never be written apart.
    pub fn save_upload(&self, id: &UploadKey, session: &UploadSession) -> Result<()> {
        let planned = self.plan_save_upload(id, session)?;
        self.engine().apply(&planned.batch)?;
        Ok(())
    }

    pub fn plan_save_upload(&self, id: &UploadKey, session: &UploadSession) -> Result<Planned<()>> {
        let mut batch = WriteBatch::new();
        batch.put(keys::upload(id), encode(session)?);
        Ok(Planned { outcome: (), batch })
    }

    /// Drop a session. Used by both cancel and commit - a committed upload's
    /// session is as finished as an abandoned one's.
    pub fn delete_upload(&self, id: &UploadKey) -> Result<()> {
        let planned = self.plan_delete_upload(id);
        self.engine().apply(&planned.batch)?;
        Ok(())
    }

    pub fn plan_delete_upload(&self, id: &UploadKey) -> Planned<()> {
        let mut batch = WriteBatch::new();
        batch.delete(keys::upload(id));
        Planned { outcome: (), batch }
    }

    /// Every repo id an in-flight upload holds, for purge.
    ///
    /// Purge must treat these as live: retiring an interner entry that an
    /// unfinished upload still names would leave the session unable to resolve
    /// its own repository. The scan is over `U`, which is bounded by
    /// concurrency rather than by the size of the registry, so it is the one
    /// list here that does not need a cursor.
    pub fn live_upload_repos(&self, limit: usize) -> Result<Vec<RepoId>> {
        let page = self.engine().scan(&keys::uploads(), None, limit)?;
        let mut repos = Vec::new();
        for (_, value) in &page.entries {
            let session: UploadSession = decode(value, "UploadSession")?;
            if !repos.contains(&session.repo) {
                repos.push(session.repo);
            }
        }
        Ok(repos)
    }
}

impl Registry {
    /// Parse the text form of an upload id into its key bytes.
    ///
    /// Hyphenated or bare hex, case-insensitive - the id summ mints is a
    /// hyphenated v4 UUID, but a client only ever echoes back the `Location`
    /// it was given, so accepting both costs nothing and rejects everything
    /// else.
    pub fn parse_upload_id(id: &str) -> Result<UploadKey> {
        let mut raw = [0u8; 16];
        let mut nibbles = id.bytes().filter(|b| *b != b'-');
        for slot in raw.iter_mut() {
            let (hi, lo) = (nibbles.next(), nibbles.next());
            match (hi.and_then(hex), lo.and_then(hex)) {
                (Some(hi), Some(lo)) => *slot = (hi << 4) | lo,
                _ => return Err(malformed(id)),
            }
        }
        if nibbles.next().is_some() {
            return Err(malformed(id));
        }
        Ok(raw)
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn malformed(id: &str) -> RegistryError {
    RegistryError::invalid(format!("malformed upload id {id:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_parses_hyphenated_or_bare_and_nothing_else() {
        let hyphenated = "58fd54e5-1720-4ed9-a39d-ff9800ac6790";
        let bare = "58fd54e517204ed9a39dff9800ac6790";
        assert_eq!(
            Registry::parse_upload_id(hyphenated).unwrap(),
            Registry::parse_upload_id(bare).unwrap(),
            "the hyphens are presentation, not identity"
        );
        assert_eq!(
            Registry::parse_upload_id(&hyphenated.to_uppercase()).unwrap(),
            Registry::parse_upload_id(bare).unwrap()
        );

        for bad in [
            "",
            "58fd54e5",
            "58fd54e517204ed9a39dff9800ac6790aa",
            "58fd54e517204ed9a39dff9800ac679g",
            "../../etc/passwd",
        ] {
            assert!(
                Registry::parse_upload_id(bad).is_err(),
                "{bad:?} must not parse as an upload id"
            );
        }
    }
}
