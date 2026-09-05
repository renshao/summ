//! Discovery queries - the extension API's read side.
//!
//! None of this is in the Distribution Spec, which is the point: `_catalog` was
//! removed before 1.0.0 and nothing standard answers "what is in here". Because
//! nothing external validates these, their pagination is stated explicitly and
//! tested here.
//!
//! Every query takes a cursor and a limit, including the two that look like
//! aggregates. There is no stored total for a repo's size or its manifest
//! count, deliberately: maintaining one would be a read-modify-write on the
//! push path, which is the one thing the schema exists to avoid. A caller that
//! wants a total folds the pages, and a caller that wants a screenful takes the
//! first page.

use summ_core::{keys, Digest, ManifestRecord, RepoBlobRecord};

use crate::codec::decode;
use crate::error::{RegistryError, Result};
use crate::registry::Registry;
use crate::suffix;

/// How many name keys a substring search reads per underlying scan.
///
/// The walk is one seek and a sequential read either way; this only decides how
/// often it returns to ask for more, and a batch of names is a few kilobytes.
const SEARCH_STEP: usize = 512;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoList {
    pub repos: Vec<String>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestList {
    pub manifests: Vec<ManifestRecord>,
    pub next: Option<Digest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DigestList {
    pub digests: Vec<Digest>,
    pub next: Option<Digest>,
}

/// One page of a repo's blob set. Fold `blobs` and `bytes` across pages for a
/// total; there is no stored aggregate to read instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoUsagePage {
    pub blobs: u64,
    pub bytes: u64,
    pub next: Option<Digest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestCountPage {
    pub manifests: u64,
    pub next: Option<Digest>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagCountPage {
    pub tags: u64,
    pub next: Option<String>,
}

/// A manifest that references a blob, and the repo it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobReference {
    pub repo: String,
    pub manifest: Digest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobReferenceList {
    pub references: Vec<BlobReference>,
    pub next: Option<BlobReference>,
}

impl Registry {
    /// Every repository, in name order.
    ///
    /// Pages over `n <name>`, never over `i <id>`. `n` is name-ordered and `i`
    /// is insertion-ordered, and both `_catalog`'s de-facto contract and any
    /// stable cursor need the former. Using `i` would be faster to write and
    /// would silently return repositories in creation order.
    pub fn list_repos(&self, start_after: Option<&str>, limit: usize) -> Result<RepoList> {
        self.search_repos("", start_after, limit)
    }

    /// Repositories whose name begins with `prefix`, in name order.
    ///
    /// This is [`Registry::list_repos`] with a longer scan prefix and nothing
    /// else: `n <name>` is the name itself appended to one type byte, so a name
    /// prefix *is* a key prefix, and the search costs one seek and a sequential
    /// walk of exactly the matching run. No index, no filter, no scan of the
    /// non-matching remainder.
    ///
    /// The empty prefix is the whole range, which is why `list_repos` is a call
    /// to this rather than the other way round.
    pub fn search_repos(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<RepoList> {
        let prefix = keys::repo_by_name(prefix);
        let cursor = start_after.map(keys::repo_by_name);
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut repos = Vec::with_capacity(page.keys.len());
        for key in &page.keys {
            repos.push(
                keys::parse_repo_name(key)
                    .ok_or_else(|| RegistryError::corrupt("repo name key"))?
                    .to_string(),
            );
        }
        let next = match page.next.as_deref() {
            Some(key) => Some(
                keys::parse_repo_name(key)
                    .ok_or_else(|| RegistryError::corrupt("repo cursor"))?
                    .to_string(),
            ),
            None => None,
        };
        Ok(RepoList { repos, next })
    }

    /// Repositories whose name *contains* `needle`, in name order.
    ///
    /// Unlike [`Registry::search_repos`] this cannot ride the key order: a
    /// substring may start anywhere in the name, so no key prefix brackets the
    /// matches and every name in the range has to be looked at. It is still the
    /// cheap kind of scan - an `n <name>` key is one type byte plus the name and
    /// its value is empty, so `scan_keys` reads keys and decodes nothing - but
    /// it is a walk of the catalogue rather than a seek into it, and that is the
    /// whole difference between this and the prefix search.
    ///
    /// Two bounds keep a call finite. Matches are gathered one past `limit`, so
    /// `next` is `None` only once the scan has *proved* nothing follows, never
    /// because a page came back full. And the walk gives up after
    /// [`RegistryOptions::search_ceiling`] keys, returning a short page whose
    /// cursor points at where it stopped - the same bargain
    /// [`Registry::untagged_manifests`] makes, and the reason a caller pages
    /// until `next` is `None` rather than until a page arrives empty.
    ///
    /// [`RegistryOptions::search_ceiling`]: crate::RegistryOptions::search_ceiling
    pub fn search_repos_containing(
        &self,
        needle: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<RepoList> {
        // The empty needle matches every name, and *that* the key order can
        // bracket: it is the whole range, so hand it back to the seek path.
        if needle.is_empty() {
            return self.search_repos("", start_after, limit);
        }

        let ceiling = self.options().search_ceiling;
        let prefix = keys::repos_by_name();
        let mut cursor = start_after.map(keys::repo_by_name);
        let mut repos: Vec<String> = Vec::new();
        let mut examined = 0usize;

        let next = 'walk: loop {
            // Never read past the budget. Asking for a whole step and stopping
            // afterwards would make the ceiling a multiple of the step rather
            // than the count it claims to be.
            let step = SEARCH_STEP.min(ceiling.saturating_sub(examined));
            let page = self.engine().scan_keys(&prefix, cursor.as_deref(), step)?;

            for key in &page.keys {
                let name = keys::parse_repo_name(key)
                    .ok_or_else(|| RegistryError::corrupt("repo name key"))?;
                examined += 1;
                if !name.contains(needle) {
                    continue;
                }
                if repos.len() == limit {
                    // The match one past the page. It is not served; it exists
                    // only to prove there is a further page, which is the sole
                    // reason this page carries a cursor at all.
                    break 'walk repos.last().cloned();
                }
                repos.push(name.to_string());
            }

            match page.next {
                // The range ended. "Nothing further matches" is now proved.
                None => break None,
                // The ceiling stopped the walk mid-range: a short page, and a
                // cursor at the last name examined rather than the last matched.
                Some(key) if examined >= ceiling => {
                    break Some(
                        keys::parse_repo_name(&key)
                            .ok_or_else(|| RegistryError::corrupt("repo cursor"))?
                            .to_string(),
                    );
                }
                next => cursor = next,
            }
        };

        Ok(RepoList { repos, next })
    }

    /// Every manifest in a repo, in digest order.
    pub fn list_manifests(
        &self,
        repo: &str,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<ManifestList> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::manifests_in_repo(repo_id);
        let cursor = start_after.map(|d| keys::manifest(repo_id, d));
        let page = self.engine().scan(&prefix, cursor.as_deref(), limit)?;

        let mut manifests = Vec::with_capacity(page.entries.len());
        for (_, value) in &page.entries {
            manifests.push(decode::<ManifestRecord>(value, "ManifestRecord")?);
        }
        Ok(ManifestList {
            manifests,
            next: cursor_digest(page.next.as_deref(), "manifest cursor")?,
        })
    }

    /// Manifests in a repo with no tag pointing at them - the reclaimable set.
    ///
    /// `M` minus `G`: one prefix-existence check per manifest, which is a seek
    /// rather than a scan and is the same check purge makes. Like the filtered
    /// referrers query, the cursor advances over `M` rather than over the
    /// results, so a page may come back short with `next` still set.
    pub fn untagged_manifests(
        &self,
        repo: &str,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<DigestList> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::manifests_in_repo(repo_id);
        let cursor = start_after.map(|d| keys::manifest(repo_id, d));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut digests = Vec::new();
        for key in &page.keys {
            let digest = suffix::digest_after_repo(key)
                .ok_or_else(|| RegistryError::corrupt("manifest key"))?;
            if !self
                .engine()
                .exists_prefix(&keys::tags_of_manifest(repo_id, &digest))?
            {
                digests.push(digest);
            }
        }
        Ok(DigestList {
            digests,
            next: cursor_digest(page.next.as_deref(), "manifest cursor")?,
        })
    }

    /// Blob count and byte total for a repo, one page of `P` at a time.
    pub fn repo_usage(
        &self,
        repo: &str,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<RepoUsagePage> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::blobs_in_repo(repo_id);
        let cursor = start_after.map(|d| keys::repo_blob(repo_id, d));
        let page = self.engine().scan(&prefix, cursor.as_deref(), limit)?;

        let mut usage = RepoUsagePage {
            next: cursor_digest(page.next.as_deref(), "repo blob cursor")?,
            ..RepoUsagePage::default()
        };
        for (_, value) in &page.entries {
            let record: RepoBlobRecord = decode(value, "RepoBlobRecord")?;
            usage.blobs += 1;
            usage.bytes += record.size;
        }
        Ok(usage)
    }

    /// Manifest count for a repo, one page of `M` at a time.
    pub fn count_manifests(
        &self,
        repo: &str,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<ManifestCountPage> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::manifests_in_repo(repo_id);
        let cursor = start_after.map(|d| keys::manifest(repo_id, d));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;
        Ok(ManifestCountPage {
            manifests: page.keys.len() as u64,
            next: cursor_digest(page.next.as_deref(), "manifest cursor")?,
        })
    }

    /// Tag count for a repo, one page of `T` at a time.
    ///
    /// Counts keys without decoding the names, which is the only thing that
    /// separates it from [`Registry::list_tags`] - a caller that only wants a
    /// number should not pay to allocate a `String` per tag on the way to
    /// discarding it.
    pub fn count_tags(
        &self,
        repo: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<TagCountPage> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::tags_in_repo(repo_id);
        let cursor = start_after.map(|t| keys::tag(repo_id, t));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;
        let next = match page.next.as_deref() {
            Some(key) => Some(
                keys::parse_tag_suffix(key)
                    .ok_or_else(|| RegistryError::corrupt("tag cursor"))?
                    .to_string(),
            ),
            None => None,
        };
        Ok(TagCountPage {
            tags: page.keys.len() as u64,
            next,
        })
    }

    /// Which manifests reference a blob, across the whole registry.
    ///
    /// Registry-wide on purpose: `R <digest> <repo> <manifest>` is keyed by the
    /// blob first precisely so that "what shares this layer" is one scan rather
    /// than one per repository. Repo names are resolved through the interner
    /// so the caller never sees a `RepoId`.
    pub fn manifests_referencing_blob(
        &self,
        digest: &Digest,
        start_after: Option<&BlobReference>,
        limit: usize,
    ) -> Result<BlobReferenceList> {
        let prefix = keys::blob_refs(digest);
        // A cursor naming a repo that no longer exists cannot be positioned, so
        // the scan restarts. That is the only honest answer, and better than
        // silently skipping the rest of the range.
        let cursor = match start_after {
            Some(after) => self
                .lookup_repo(&after.repo)?
                .map(|id| keys::blob_ref(digest, id, &after.manifest)),
            None => None,
        };
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut references = Vec::with_capacity(page.keys.len());
        for key in &page.keys {
            references.push(self.blob_reference(key)?);
        }
        let next = match page.next.as_deref() {
            Some(key) => Some(self.blob_reference(key)?),
            None => None,
        };
        Ok(BlobReferenceList { references, next })
    }

    /// Indexes in a repo that list this manifest as one of their children.
    pub fn parents_of_manifest(
        &self,
        repo: &str,
        child: &Digest,
        start_after: Option<&Digest>,
        limit: usize,
    ) -> Result<DigestList> {
        let repo_id = self.require_repo(repo)?;
        let prefix = keys::parents_of(repo_id, child);
        let cursor = start_after.map(|d| keys::child_parent(repo_id, child, d));
        let page = self.engine().scan_keys(&prefix, cursor.as_deref(), limit)?;

        let mut digests = Vec::with_capacity(page.keys.len());
        for key in &page.keys {
            digests.push(
                suffix::second_digest_after_repo(key)
                    .ok_or_else(|| RegistryError::corrupt("child-parent key"))?,
            );
        }
        let next = match page.next.as_deref() {
            Some(key) => Some(
                suffix::second_digest_after_repo(key)
                    .ok_or_else(|| RegistryError::corrupt("child-parent cursor"))?,
            ),
            None => None,
        };
        Ok(DigestList { digests, next })
    }

    fn blob_reference(&self, key: &[u8]) -> Result<BlobReference> {
        let (repo_id, manifest) =
            suffix::blob_ref_target(key).ok_or_else(|| RegistryError::corrupt("blob ref key"))?;
        Ok(BlobReference {
            repo: self.repo_name(repo_id)?,
            manifest,
        })
    }
}

fn cursor_digest(key: Option<&[u8]>, what: &str) -> Result<Option<Digest>> {
    match key {
        Some(key) => Ok(Some(
            suffix::digest_after_repo(key).ok_or_else(|| RegistryError::corrupt(what))?,
        )),
        None => Ok(None),
    }
}
