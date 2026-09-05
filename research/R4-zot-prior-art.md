# R4 — zot as prior art

**Question:** What has zot already solved that summ would otherwise rediscover
the hard way?

**Sources read:** `../../zot` at `b689b06` (2026-09-02) — `pkg/meta/{types,common,parse,hooks,proto,boltdb,redis,dynamodb}`,
`pkg/storage/{types,common,local,s3,imagestore,cache,gc,scrub}`, `pkg/retention`,
`pkg/api/{routes,constants}`, `pkg/common/common.go`. Plus `../../harbor`
(`src/pkg/{artifact,blob,accessory}`, `src/jobservice/job/impl/gc`) as a second
data point.

**Headline:** the answer is largely inverted. zot is the closest architectural
relative in existence — an embedded metadata DB beside content-addressed blobs —
and on the two questions summ has already decided (fan-in storage, and the
`Move` vs `commit_upload` driver primitive) **zot made the choice summ rejected,
in both cases.** So the transferable value is not "copy their design"; it is
(a) hard confirmation that the two central bets are right, and (b) a list of
specific things they hit that summ's design does *not* automatically avoid.

Sections 1–5 answer the brief. Section 6 is the list that matters: things we
would have got wrong.

---

## 1. `pkg/meta/` — their metadata layer

### 1.1 The data model, and where it goes wrong

zot's metaDB has three logical tables (bolt buckets / dynamo tables / redis
hashes, `pkg/meta/boltdb/buckets.go:1-15`):

| zot | keyed by | value |
|---|---|---|
| `ImageMeta` | manifest digest | protobuf: the whole manifest + its parsed config, or the whole index |
| `RepoMeta` | repo name | protobuf: **every tag, every per-digest statistic, every per-digest signature set, every per-digest referrer list for the whole repo** |
| `RepoBlobsMeta` | repo name | protobuf: **a map of every blob digest in the repo → size, vendors, platforms, sub-blob list** |

The `RepoMeta` message is `pkg/meta/proto/meta/meta.proto:42-62`:

```proto
message RepoMeta {
    string                     Name = 1;
    map<string, TagDescriptor> Tags = 2;
    map<string, DescriptorStatistics> Statistics = 3;
    map<string, ManifestSignatures>   Signatures = 4;
    map<string, ReferrersInfo>        Referrers  = 5;
    ...
```

Every relationship summ stores as an edge key — `T`/`G` (tags), `F`
(referrers), `R`/`P` (blob membership), `S` (index children) — zot stores
**inline, in one value per repo**. `RepoBlobs.Blobs[d].SubBlobs`
(`meta.proto:64-77`) is the manifest→child/layer DAG, also inline.

This is *exactly* the pattern PLAN.md § "The scale constraint that shapes
everything" identifies as fatal, and zot pays every predicted cost:

- **Read-modify-write on every push.** `BoltDB.SetRepoReference`
  (`pkg/meta/boltdb/boltdb.go:186-334`) loads the entire repo protobuf,
  mutates four maps, and rewrites it. So does `RemoveRepoReference`
  (`boltdb.go:1441-1545`).
- **O(repo) work per push, not O(1).** `common.AddImageMetaToRepoMeta`
  (`pkg/meta/common/common.go:177-272`) calls `recalculateAggregateFields`
  (`common.go:337-372`) on **every tagged push**, which BFS-walks *every tag in
  the repo* and *every blob transitively reachable from it* to recompute
  `repoMeta.Size`/`Platforms`/`Vendors`. `RemoveImageFromRepoMeta`
  (`common.go:274-335`) does the same walk on every delete, and rebuilds the
  whole `RepoBlobs.Blobs` map from scratch. At summ's stated bound of 10M
  manifests in one repo this is not slow, it is non-terminating.
- **A write on the read path.** `UpdateStatsOnDownload`
  (`boltdb.go:1286-1349`) read-modify-writes the *entire repo protobuf* to
  bump one download counter — on **every pull**. Every pull of every image in a
  repo serialises against every other.
- **A hard ceiling on DynamoDB.** `RepoMeta` is one DynamoDB item
  (`pkg/meta/dynamodb/dynamodb.go:33-44`). DynamoDB items are capped at 400 KB.
  There is no chunking anywhere in that file. A repo with a few thousand tags
  and their signature/referrer metadata simply stops being writable.
- **Unbounded reads.** `RedisDB.SearchRepos` / `FilterRepos` / `CountRepos` /
  `GetAllRepoNames` (`pkg/meta/redis/redis.go:932, 1165, 2106, 2124`) all call
  `HGETALL` on the repo-meta hash — every repo's full metadata into memory in
  one round trip. `DynamoDB.GetAllRepoNames`
  (`pkg/meta/dynamodb/dynamodb.go:95-119`) is a full table `Scan`.

**Verdict:** zot's data model is a *direct* instance of the anti-pattern summ's
schema was written to avoid, in a production registry, at v2. The one-key-per-edge
rule is not over-engineering; it is the difference between the two projects.

### 1.2 What they store that we don't

Genuinely missing from summ, in rough order of how much it will hurt:

| zot has | summ | needed? |
|---|---|---|
| `ReferrerInfo{ArtifactType, Annotations, Size, MediaType}` on the referrer edge (`meta.proto:91-99`) | `F` edge is valueless; `ManifestRecord` has no `artifact_type` and no `annotations` | **Yes — blocking.** See §4. |
| `DescriptorStatistics{PushTimestamp, PushedBy, LastPullTimestamp, DownloadCount}` (`meta.proto:80-85`) | nothing | **Yes**, for retention. See §6.6. |
| `TagDescriptor.TaggedTimestamp` (`meta.proto:10-14`) | `T` value is a bare digest | Probably. |
| `RepoMeta.Size` / `Platforms` / `Vendors`, maintained aggregates | computed on demand | Useful for the extension API; must **not** be a maintained aggregate. |
| `RepoLastUpdated` sub-bucket (`buckets.go:12-15`), keyed separately from `RepoMeta` | nothing | Yes if we ever add a rebuild path — it is how they skip re-parsing an unchanged repo. |
| Parsed image **config** stored alongside the manifest (`ManifestMeta.Config`, `meta.proto:22-27`) | nothing | Only for search/UI. Skip. |
| A `DBVersion` key and a patch-list migration framework (`pkg/meta/version/`) | **nothing** | Yes. See §6.9. |

Note the `SubBlobs` DAG (`meta.proto:71-77`) plus BFS-with-visited-set for size
(`common.go:337-372`): the *algorithm* is right for multi-arch de-duplication
(an index and its children share layers; a naive sum double-counts). summ has
the same graph in `S` + `ManifestRecord.layers`, so the BFS transfers even
though the storage does not.

### 1.3 Three backends, and what that says about `MetaEngine`

zot's `MetaDB` (`pkg/meta/types/types.go:67-181`) is a **~40-method
domain-level** interface: `SetRepoReference`, `SearchTags`, `AddManifestSignature`,
`IncrementRepoStars`, `UpdateStatsOnDownload`… The consequence is that all
registry semantics are re-implemented once per backend:

```
pkg/meta/boltdb/boltdb.go    2254 lines
pkg/meta/redis/redis.go      2474 lines
pkg/meta/dynamodb/dynamodb.go 2549 lines
```

~7 300 lines expressing the same logic three times, sharing only the
protobuf-level helpers in `pkg/meta/common/`. The abstraction is at the wrong
altitude, and it costs them: every semantic fix has to land three times.

summ's `MetaEngine` (`summ-meta/src/engine.rs:91-107`) is four methods —
`get`, `scan`, `exists_prefix`, `apply(WriteBatch)` — with all registry
semantics above the line. Comparing: **the altitude is right and this is the
single clearest place summ is better designed than the prior art.** RocksDB and
redb are ~200 lines each and mean the same thing.

But the three-backend exercise does expose real gaps:

- **Redis needs distributed locks that Bolt does not.** `redis.go` wraps almost
  every mutation in `withRSLocks` (redsync) — `SetRepoReference` at
  `redis.go:775`, and ~20 more. That entire mechanism exists *only* because the
  data model requires read-modify-write. summ's insert-only batches need no
  locking at any backend. Worth stating explicitly in PLAN.md: **the absence of
  RMW is what makes the engine boundary portable**, not just fast.
- **Under-abstracted: no batched read.** Answering `GET /referrers` needs one
  prefix scan plus N point lookups if `F` stays valueless. RocksDB has
  `multi_get`; there is no way to reach it. (Better fix: put the payload in the
  `F` value — §4.)
- **Under-abstracted: no key-only scan.** `Page.entries` is
  `Vec<(Vec<u8>, Vec<u8>)>` (`engine.rs:83-89`). Purge scans millions of
  valueless `R`/`P`/`G` keys and allocates an empty `Vec` per row. A
  `scan_keys` returning only keys is cheap to add and material at purge scale.
- **Under-abstracted: no snapshot.** `exists_prefix` then `apply` is a TOCTOU
  against a concurrent push, and `WriteBatch` has no compare-and-set (correctly
  — CAS would break replayability). This means **offline purge is not merely a
  v1 convenience, it is forced by the trait shape.** The upgrade path is
  therefore *not* "add a conditional op"; it is a grace period (§3).
- **`DeletePrefix` is weaker than PLAN.md claims.** `Put`/`Delete` are
  genuinely idempotent. `DeletePrefix` is only safe under *in-order* replay of a
  log suffix — replayed out of order, or after a later batch inserted keys under
  that prefix, it destroys them. PLAN.md § "Replication and the WAL" says
  batches are idempotent full stop. Tighten that sentence to "idempotent under
  in-order replay", or the HA claim is subtly false.

### 1.4 The thing that should worry us most: their DB is a cache, ours is not

`pkg/meta/parse.go` exists to **rebuild the entire metaDB by walking storage**
(`parseStorage:49-119`, `parseRepo:221-317`). zot's metaDB is a derived index.
Blow it away and `ParseStorage` reconstructs it from `index.json` + manifest
blobs. That is why every migration patch list in `pkg/meta/version/patches.go`
is **empty** — they never need to migrate; they can always rebuild.

summ has no such property. Manifest bytes live *only* in RocksDB under `B`, and
tags live only under `T`. **A lost or corrupt RocksDB is a dead registry with a
full disk of unidentifiable blobs.** This is the largest unlisted risk in
PLAN.md. See §6.9.

Note also that "rebuild from storage" does not actually scale for zot either:
`parseStorage` walks every repo on every start. The `FastRestartStamp`
machinery (`parse.go:121-190`) — a binary-identity + storage-config fingerprint
that lets a clean shutdown skip the walk — is a recent band-aid on exactly that.
Worth stealing the *pattern* (cheap proof a rebuild can be skipped) if summ ever
gains a rebuild path.

---

## 2. `pkg/storage/` — the driver interface

### 2.1 They did not reach our conclusion. They inherited distribution's mistake wholesale.

`pkg/storage/types/types.go:78-93` defines zot's `Driver`. It has `Move`. It has
`Writer(path, append) → storagedriver.FileWriter`. It has `Link`. It is
distribution's driver interface with a thinner signature, and
`pkg/storage/s3/driver.go:15-21` makes this literal — zot's S3 driver *is* a
struct wrapping `distribution/registry/storage/driver.StorageDriver`, with
`Move` forwarding straight through (`s3/driver.go:87-89`).

So on S3, `FinishBlobUpload` (`pkg/storage/imagestore/imagestore.go:1126-1209`)
does the following to commit one layer:

1. `Writer(src, append=true)` then `Commit()` — completes the S3 multipart
   upload at `<repo>/.uploads/<uuid>` (`imagestore.go:1134-1151`).
2. `getBlobDigest(is, src, algo)` (`imagestore.go:1153`, defined at
   `imagestore.go:2241-2256`) — opens a `Reader` on that object and hashes it.
   **This is a full GET of the entire blob back out of S3, purely to verify a
   digest they streamed past moments earlier.**
3. `Move(src, dst)` (`imagestore.go:1200`) — which on S3 is
   `CopyObject` + `Delete`, i.e. **a second full copy of the blob**.

Three full passes over a multi-gigabyte layer to commit it. PLAN.md § "Blob
storage" deviation 2 calls `Move` "the single most important trait-design lesson
to take from distribution, and it is a lesson by counter-example." zot is the
counter-example's counter-example: a modern registry that took the interface
unexamined and pays for it on every push.

**Conclusion: `commit_upload` stands, and the resumable-hashing plan (PLAN.md
deviation 4, `sha2` 0.11 `SerializableState`) is worth more than PLAN.md
claims** — it eliminates step 2 entirely, which zot has not eliminated at all.

### 2.2 Chunked upload → S3 multipart

zot has **no upload session state of its own.** The mapping is:

- `NewBlobUpload` (`imagestore.go:967-994`) opens `Writer(path, append=false)`
  and immediately closes it — that is the "create multipart upload".
- `PutBlobChunk` (`imagestore.go:1082-1105`) reopens `Writer(path, append=true)`
  per chunk, and validates `from != file.Size()` → `ErrBadUploadRange`. The
  offset is read back **from the driver**, not from a session record.
- `FinishBlobUpload` calls `Commit()`.
- `DeleteBlobUpload` (`imagestore.go:1422-1443`) calls `writer.Cancel()`, which
  is `AbortMultipartUpload`.

Resumption therefore works only because the driver's `Writer(append=true)` can
report a size — i.e. it depends entirely on distribution's S3 driver internally
persisting multipart part lists. There is no hasher state, hence the
re-read-to-hash in §2.1.

summ's plan (`U <uuid>` → `UploadSession` with a 104-byte serialised hasher
state) is strictly better on all three axes: no re-read, resumable on any
process, and the session is transactional with the rest of the metadata batch.
**One gap:** `UploadSession` (`summ-core/src/types.rs:62-71`) does not carry the
hasher state field yet, nor the multipart upload-id the S3 driver will need.
Both belong there.

Two real constraints their code surfaces that summ's `UploadSession` must
respect:

- Per the spec a chunked upload's chunks must arrive in order and `PATCH`
  must 416 on a gap. zot enforces this against the *driver's* reported size.
  summ enforces it against `UploadSession.offset`, which is correct — but the
  two must not drift: if a chunk write to the blob store succeeds and the
  metadata batch fails, the driver is ahead of `offset`. Ordering rule: write
  the chunk, then commit `offset`; on retry, a client re-`PATCH`ing at the old
  offset must be able to overwrite. That works for a filesystem append but
  **not** for S3 multipart, where a part number is consumed. Track the part
  number in the session and make it idempotent per-offset.

### 2.3 `cache/` — what it is, and why summ needs nothing like it

`pkg/storage/types/cache.go:7-27` is a `digest → [path]` index with three
implementations (`pkg/storage/cache/{boltdb,dynamodb,redis}.go`, ~1 100 lines).
It exists for exactly one reason: **zot stores blobs per-repo**
(`BlobPath = <root>/<repo>/blobs/<algo>/<hex>`, `imagestore.go:1446-1448`,
following the OCI image-layout spec), so the same layer pushed to 100 repos is
100 copies. Dedupe then re-links them:

- On a filesystem, `Link` is a hardlink (`pkg/storage/local/driver.go:288-305`).
- **On S3, `Link` writes a zero-byte object** (`pkg/storage/s3/driver.go:108-113`):

  > "Because s3 doesn't support symlinks, wherever the storage will encounter an
  > empty file, it will get the original one from cache."

The cache DB is then the only thing that knows which zero-byte object points
where. That single hack propagates through the whole codebase, and its
consequences are visible as scar tissue:

- `deleteBlobChecked` (`imagestore.go:2124-2239`) must, before deleting a blob,
  ask the cache whether this path is the *content* copy or a placeholder, and
  if it is the content copy, **`Move` the bytes to the next placeholder**
  (`imagestore.go:2190-2211`).
- A whole `dedupeRebuildDone` atomic gate exists so that deletes are *deferred*
  at startup until a restore walk finishes, because until then a placeholder's
  content copy cannot be identified (`imagestore.go:2158-2166`, `2215-2222`).
  Getting this wrong means silent data loss.
- A `_restore_complete` marker file with three states is needed just to skip
  that walk (`pkg/storage/constants/constants.go:31-40`).
- `local/driver.go:297-302`: hardlinks share an inode, so a link inherits the
  *original's* mtime, and GC's mtime grace period would immediately reap a
  freshly linked blob. They must `os.Chtimes` explicitly.

summ's global content-addressed store (`blobs/sha256/ab/cd/ef/<hex>`,
`digest → bytes`) makes all ~1 500 lines of this unnecessary: dedupe is the
storage layout, not a subsystem. **We need no cache layer.** This is the largest
single body of complexity summ avoids, and PLAN.md's "blob storage holds bytes,
not relationships" is what buys it.

The tradeoff, stated honestly: per-repo blobs give zot cross-repo isolation and
per-repo deletion *for free* from the filesystem. summ substitutes `R`/`P` keys
for the first (PLAN.md already flags "do not serve a blob just because `L`
exists") and makes the second a registry-wide question — see §3.

---

## 3. `pkg/storage/gc/` — garbage collection

**Theirs is online, per-repo, and lock-based.** `cleanRepo`
(`pkg/storage/gc/gc.go:134-252`) takes the image store's **write lock for the
whole repo sweep** (`gc.go:142-143`) and then:

1. applies tag-retention policy,
2. removes referrers whose subject is gone, and untagged manifests,
3. prunes index entries with unknown media types (`gc.go:254-288`) and stale
   ones (`gc.go:294-...`),
4. rewrites `index.json`,
5. `deleteUnreferencedBlobs` (`gc.go:1049-1106`): `GetReferencedBlobs` walks
   every manifest in the repo transitively, `GetAllBlobs` lists the repo's blob
   dir, set-difference, then delete,
6. removes the repo if idle, then reaps stale uploads.

That is a full repo tree walk per sweep, per repo, on a scheduler
(`gc.go:82-99`), on a **randomised delay** to avoid thundering herds, inside a
**daily time window** so a sweep can't run in peak hours (`gc.go:44-50`).

### What summ gets for free

Their step 5 — the expensive one — is `O(manifests × layers)` reads per repo.
summ's equivalent is `exists_prefix(R <digest>)`: one seek per candidate blob.
The `R` index really is the difference, and it is the reason summ can consider
offline purge as a *fast* sweep rather than a multi-hour walk.

### What summ does *not* get for free, and this is the important part

**Their races, and how they solved them:**

1. **The upload-pinning race — solved by an mtime grace period, not by
   pinning.** `isBlobOlderThan` (`gc.go:1118-1138`) spares any blob whose mtime
   is within `Delay` (default 1 h, `constants.go:24`). Same for uploads
   (`gc.go:1028-1031`) and for idle-repo removal (`imagestore.go:2069-2080`).
   PLAN.md Risk 4 names "upload-session pinning via `U` keys plus an mtime grace
   period" as the upgrade path — **zot confirms the grace period is the load-
   bearing half and pinning is the optimisation.** Grace preserves batch
   idempotence; a conditional delete would not.

2. **Taking a lock before reading a request body deadlocks against slow
   clients.** `openBlobUploadWriter`, `imagestore.go:1039-1041`:

   > "NewBlobUpload already created the repo. Avoid InitRepo: it takes the store
   > write lock before the request body is read, which races
   > http.Server.ReadTimeout."

   A chunk `PATCH` that grabbed a repo-wide lock and then blocked on the socket
   would stall GC for as long as the client was slow. Real bug, real fix.

3. **A blob can finish uploading *after* GC deleted its repo.**
   `FinishBlobUpload`, `imagestore.go:1174-1180`:

   > "Chunk PUT/PATCH no longer call InitRepo (that lock stalled unread bodies
   > vs GC). Recreate the OCI layout here, while we already hold the store lock
   > for the move, so a blob finished after GC deleted the repo is still a
   > walkable repository."

   summ's analogue: purge deletes `n <name>`/`i <id>` for an empty repo while an
   `U` session still holds that `RepoId`. On commit the batch writes `P <repo>
   <digest>` under an id no longer in the interner — an orphan key range that
   nothing will ever scan or reclaim. **Purge must not free a `RepoId` while any
   `U` session references it.**

4. **Fail closed on storage errors.** `gc.go:1123-1126`:

   > "ImageStore.StatBlob maps any underlying Stat failure (including transient
   > S3/network errors) to ErrBlobNotFound, so treating 'missing' as GC-eligible
   > would risk deleting live index rows during storage blips."

   And `imagestore.go:2088` "fail closed on partial sweeps and eventually-
   consistent listings." Both are about S3, both are one-line policies that
   prevent data loss.

**Harbor, for comparison, does online GC properly** and it is instructive how
much machinery that takes: a per-blob status state machine
(`src/pkg/blob/models/blob.go:37-68`: `none → delete → deleting → trash`, with
`delete → none` when a client asks for the blob again), a `version` column with
optimistic CAS on every transition (`src/pkg/blob/dao/dao.go:185-196`), a
2-hour default time window (`src/jobservice/job/impl/gc/garbage_collection.go:143`),
**and it still puts the registry into read-only mode for the sweep**
(`garbage_collection.go:43,353-452`).

**Verdict: offline purge for v1 is not a shortcut, it is what the two most
mature comparable systems both effectively do.** The upgrade path is
grace-period-first, and the design of that grace period is a schema question
(§6.4), not a locking question.

---

## 4. Referrers, signatures, artifacts — and the `F` edge does *not* hold up as-is

### 4.1 How zot indexes them

Three parallel mechanisms, because the real world has three:

1. **OCI 1.1 `subject`.** On push, `SetRepoReference` appends to
   `RepoMeta.Referrers[subject.Digest]` (`boltdb.go:226-256`). `GetReferrersInfo`
   (`boltdb.go:1243-1330`) reads that list and filters by artifact type.
2. **A storage fallback** for when there is no metaDB: `common.GetReferrers`
   (`pkg/storage/common/common.go:803-912`) **reads every manifest blob in the
   repo** and checks each one's `subject` field. `O(repo)` per referrers request.
3. **Legacy cosign tag-based artifacts.** `sha256-<hex>.sig` and
   `sha256-<hex>.sbom` tags, matched by regex
   (`pkg/common/common.go:41-55`), with the subject digest *parsed out of the
   tag name* (`gc.go:1140-1147`, `pkg/meta/parse.go:573-591`). Also notation
   (`ArtifactTypeNotation`) and the newer sigstore-bundle artifact type
   (`common.go:30-38`).
4. The **referrers fallback tag schema** (`sha256-<hex>` index tags) is
   recognised and *excluded* from the tag list (`IsReferrersTag`,
   `pkg/common/common.go:130-136`; used at `parse.go:249-254, 268-270`).

### 4.2 Does summ's `F <repo> <subject> <referrer>` hold up?

**Shape: yes. Payload: no — this is the clearest concrete defect found.**

The referrers response is an image index whose `manifests[]` entries require
`mediaType`, `size`, `digest`, **`artifactType`**, and **`annotations`** (zot
constructs exactly these at `common.go:870-876` and `896-902`). The API also
supports `?artifactType=` filtering with an `OCI-Filters-Applied: artifactType`
response header (`pkg/api/routes.go:683-686`).

summ today:

- `F` (`summ-core/src/keys.rs:187-197`) is **valueless**.
- `ManifestRecord` (`summ-core/src/types.rs:35-54`) has `media_type` and `size`
  but **no `artifact_type` and no `annotations`.**

So summ **cannot construct a spec-compliant referrers response from its current
schema at all**, and even after adding the fields to `ManifestRecord`, answering
one referrers request would be a prefix scan plus N point lookups plus N
decodes — with the artifactType filter applied *after* all that work.

**Recommendation:** put the payload in the `F` value.

```
F <repo> <subject> <referrer>  ->  ReferrerRecord { media_type, artifact_type,
                                                    size, annotations }
```

This is bounded fan-out (one referrer's own descriptor), so it does not violate
the no-growing-values rule; it makes the whole endpoint a single ordered prefix
scan; it makes `?artifactType=` a filter applied during the scan; and it makes
the response naturally pageable, which zot's endpoint is not
(`routes.go:630-689` has no `n`/`Link`). Cap `annotations` at a few KB.

Harbor independently reached the same denormalisation: `artifact_accessory`
(`src/pkg/accessory/dao/model.go:28-39`) is an edge row carrying
`subject_artifact_digest`, `subject_artifact_repo`, `type`, `size`, `digest`.
Two independent systems putting the descriptor on the edge is a strong signal.

### 4.3 Legacy cosign is a real gap, and it is a *purge* gap

Cosign's tag-based form (`sha256-<hex>.sig`) still dominates in the wild. Under
summ's schema such an object is **just another tagged manifest** with no
`subject`, so no `F` edge exists. Pull and push work correctly and conformance
passes. But:

- `GET /referrers/<digest>` will not list it — which is *spec-correct*, and zot
  agrees (it is surfaced through their signature index, not the referrers API).
- **Deleting the subject manifest leaves the `.sig` tag dangling forever**, with
  its layers pinned by `R`. summ's purge, which keys entirely off "is it
  tagged?", will never reclaim it. zot handles this explicitly: `removeReferrer`
  (`gc.go:696-730`) parses the subject out of the tag name and reaps the
  signature when the subject is gone.

**Recommendation:** either (a) synthesise an `F` edge for tags matching
`^sha256-[0-9a-f]{64}\.(sig|sbom|att)$`, so purge reaches them through the normal
path, or (b) accept the leak and document it. (a) is a few lines and is what
every mature registry ends up doing. Do not skip this silently.

### 4.4 One more thing to copy from their referrer handling

`ReferrerInfo.Count` (`meta.proto:91-99`) exists because their referrer list is
an *append*, so the same referrer pushed twice must be refcounted and
decremented on delete (`boltdb.go:1466-1491`). summ's `F` is a key — set
semantics, idempotent, no counter needed. Worth noting as a small, real win of
the edge-key model, and as a reminder that any counter in a schema is a smell.

---

## 5. Other hard-won experience

Ordered by transferability.

- **Referrers must be excluded from the tag list.** `IsReferrersTag`
  (`pkg/common/common.go:130-136`). If summ ever writes fallback-tag-schema
  index tags, `GET /tags/list` must filter them.

- **`OCI-Subject` response header on manifest PUT.**
  `pkg/api/constants/consts.go:14`; `PutImageManifest` returns the subject
  digest for exactly this (`imagestore.go:571-573, 812`). Easy to miss; part of
  OCI 1.1 conformance.

- **`?tag=` on digest pushes, and its DoS bound.**
  `MaxManifestDigestQueryTags = (8192 - 2048) / (len("tag=") + 128 + 1) == 46`
  (`consts.go:14-22`), returning **414** beyond it. And
  `MaxManifestBodySize = 4 MiB` (`consts.go:24`). Both are exactly the kind of
  detail R1 will need.

- **Multi-tag pushes need a rollback protocol when metadata and storage are
  separate.** `pkg/meta/hooks.go:22-140` — `priorTagManifestsFromMetaDB` records
  where each tag pointed *before* the push so a partial failure can restore it,
  with a careful note that calling the delete hook for a tag whose metadata was
  never applied is *unsafe*. **summ's single atomic `WriteBatch` makes this
  entire file unnecessary.** It is the best available illustration of what "one
  source of truth" buys.

- **fsync is optional in zot and off by default** (`storage/local/driver.go:24,
  433-437, 473-477`), and they never fsync the *directory* after a rename — so a
  crash can lose a committed blob's directory entry even with `commit: true`.
  summ's rule ("blobs land and fsync before the metadata batch commits") is
  stricter, but the implementation must fsync the **containing directory** after
  the rename, not just the file. That is a real omission in theirs and an easy
  one to inherit.

- **Scrub** (`pkg/storage/scrub.go:55-160`) walks every repo's `index.json`,
  re-reads every manifest, and re-hashes every layer, deliberately *without*
  holding locks for the layer check — `scrub.go:98`: "We aim for eventual
  consistency (locks, etc) since this task contends with data path." summ needs
  an equivalent, and it is cheaper: `L`/`R` give the blob set directly with no
  tree walk. Report shape (`ScrubImageResult{ImageName, Tag, Status,
  AffectedBlob, Error}`, `scrub.go:43-53`) is a reasonable model.

- **Retention policies need push *and* pull timestamps per manifest.**
  `pkg/retention/rules.go` implements `pulledWithin`, `pushedWithin`,
  `mostRecentlyPulledCount`, `mostRecentlyPushedCount`. `DaysPull.Perform`
  (`rules.go:33-48`) carries a nice subtlety: a *never-pulled but recently
  pushed* image must be retained, so the pull rule also checks push time. summ
  has neither timestamp.

- **`GetNextRepositories`** (`imagestore.go:291-374`) is zot's paginated
  catalog: a full recursive `Walk` from the storage root that **restarts from
  the beginning and skips forward to `lastRepo` on every page**. On S3, one
  catalog page is a full bucket LIST. This is precisely the pain summ exists to
  eliminate — the `n <name>` range makes it a seek. Good benchmark target for
  Package G.

- **DynamoDB `BatchGetItem` rejects the whole request on a duplicate key**
  (`dynamodb.go:2147-2151`) and caps at 100 keys. Only relevant if a remote
  engine ever appears, but a good example of the kind of thing that only shows
  up in production.

---

## 6. Things we would have got wrong

Concrete, ordered by cost of discovering them late.

### 6.1 The referrers API is unimplementable from the current schema
`F` is valueless and `ManifestRecord` has no `artifact_type`/`annotations`
(`keys.rs:187-197`, `types.rs:35-54`), but the response index requires both
(`storage/common/common.go:870-876`), and `?artifactType=` filtering plus
`OCI-Filters-Applied` depend on artifact type (`api/routes.go:683-686`).
**Fix:** give `F` a `ReferrerRecord { media_type, artifact_type, size,
annotations }` value. Bounded, keeps the endpoint a single scan, makes it
pageable. Do this before Phase 6, ideally before Package E freezes the record
types.

### 6.2 Legacy cosign `.sig`/`.sbom` tags leak forever under purge
No `subject`, so no `F` edge, so nothing connects them to the subject manifest;
deleting the subject strands the signature and pins its layers via `R`
(cf. `gc.go:696-730`, `pkg/meta/parse.go:573-591`). **Fix:** synthesise an `F`
edge from tags matching the cosign pattern, or document the leak.

### 6.3 `P <repo> <digest>` has no timestamp, so the grace period has nowhere to live
zot's entire anti-race mechanism is `mtime + Delay` (`gc.go:1118-1138`,
`1028-1031`, `imagestore.go:2069-2080`). summ's `P` value is `()`
(`keys.rs:155-159`), so applying a grace period means `stat`-ing every candidate
blob on the storage driver — on S3, one HEAD per blob. **Fix:** make `P`'s value
a small record carrying `{ size, added_at }`. `size` also removes the `L`
lookup from per-repo size stats; `added_at` is the grace clock. Both are
caller-supplied, so the batch stays deterministic.
Related: `local/driver.go:297-302` is a reminder that filesystem mtime is a
*bad* liveness signal (hardlinks share it) — putting the timestamp in metadata
is strictly better than what zot does.

### 6.4 Purge can free a `RepoId` that a live upload still holds
Deleting `n <name>`/`i <id>` for an apparently empty repo while a `U` session
references that id leaves the eventual `P`/`R` writes in an orphan id range that
nothing will scan or reclaim. zot hit the storage analogue and fixed it by
re-creating the layout under the commit lock (`imagestore.go:1174-1180`).
**Fix:** purge must scan `U` and treat any referenced `RepoId` as live. Cheap —
`U` is small.

### 6.5 Locking around a request body
Do not hold any repo-scoped lock across a body read. `imagestore.go:1039-1041`
is the bug report: an unread body under a write lock stalls everything until
`ReadTimeout`. summ has no store-wide lock today; the risk is introducing one
for purge or upload-offset validation later. Write the rule down now.

### 6.6 No timestamps anywhere means no retention story
`ManifestRecord` has no `pushed_at`; `T` values are a bare digest with no
`tagged_at`. Every retention rule zot ships needs one or both
(`retention/rules.go`), and Harbor's `artifact` table carries `push_time` and
`pull_time` (`src/pkg/artifact/dao/model.go:43-44`). **Fix:** add `pushed_at` to
`ManifestRecord` and make `T`'s value `{digest, tagged_at}`. Caller-supplied, so
WAL determinism is preserved.

**And do not follow zot on the pull side.** `UpdateStatsOnDownload`
(`boltdb.go:1286-1349`) rewrites the whole repo blob on every pull. If summ ever
wants last-pulled, it must be its own key (`A <repo> <digest>` → timestamp),
written coalesced/asynchronously off the hot path, never inside the pull's
critical section. A pull-optimised registry must not write on pull.

### 6.7 `FinishBlobUpload` must not re-read the blob to verify it
zot does (`imagestore.go:1153` → `2241-2256`) — a full S3 GET per push. summ's
resumable-hasher plan avoids this, but only if `UploadSession` actually carries
the hasher state; it does not yet (`types.rs:62-71`). Add the state field and
the S3 multipart upload-id/part-number now, while the record is cheap to change.

### 6.8 fsync the directory, not just the file
`local/driver.go:433-437` syncs the file and never the parent. A rename into a
directory whose entry is not fsynced can vanish on crash — which is exactly the
failure mode CLAUDE.md's ordering rule exists to prevent. Get this right in
Package C.

### 6.9 There is no recovery path if RocksDB is lost — and no schema version key
zot's metaDB is disposable because `parse.go` rebuilds it from storage; that is
why `version/patches.go` is empty. summ's RocksDB is authoritative: manifest
bytes only under `B`, tags only under `T`. A corrupt DB is a dead registry.
Two things follow:

1. **Add a `DBVersion` key and a migration hook before v1 ships.** zot has
   `pkg/meta/version/common.go:17` and a patch list per backend; summ has
   nothing, and retrofitting a version marker onto a populated store is
   unpleasant.
2. **Consider making `B` a cache, not the only copy.** Manifests are
   content-addressed — writing the manifest bytes to the blob store as well as
   `B` costs one small object per manifest and makes the corpus
   self-describing. It does not recover tags, so a lightweight append-only tag
   journal (which the planned WAL already is) closes the rest. This is a real
   decision, not a detail: it is the difference between "restore from backup" and
   "the registry is gone".

### 6.10 Three smaller trait-level fixes
- Add `scan_keys` (or make `Page` value-optional) — purge scans millions of
  valueless keys and allocates per row (`engine.rs:83-89`).
- Add `multi_get` if `F` stays valueless; unnecessary if §6.1 is done.
- Tighten the WAL idempotence claim in PLAN.md: `DeletePrefix` is safe only
  under in-order suffix replay, not under arbitrary reordering.

---

## 7. What is theirs alone — do not copy

- **Signature verification, trust stores, `UpdateSignaturesValidity`, cosign
  layer extraction** (`pkg/meta/parse.go:362-470`, `boltdb.go:1350-1440`). zot
  is a security product; summ is a fast registry. Index the referrer edge; do
  not verify anything.
- **Stars, bookmarks, download counts, `SearchRepos`/`SearchTags`/`RankRepoName`**
  (`pkg/meta/common/common.go:57-134`; `pkg/meta/types/types.go:79-120, 185-230`). These exist for the
  UI and drive most of the `HGETALL`/full-scan pathology in §1.1. summ's
  extension API should be cursor-paged and prefix-driven, never a rank-and-sort
  over everything.
- **Sub-stores** (`storage_controller.go:15-62`) — sharding by first path
  segment, decided at config time. Real feature, wrong axis for summ.
- **`MetaDB` as a 40-method domain interface.** Their portability story is the
  main reason it exists, and summ gets better portability from a 4-method KV
  trait. Do not let `MetaEngine` grow domain methods.
- **The `cache/` layer, `Link`, `SameFile`, `DedupeBlob`, `RunDedupeBlobs`,
  `restoreDedupedBlobs`, `dedupeRebuildDone`, `_restore_complete`.** All
  downstream of per-repo blob storage. Global CAS deletes the whole subsystem.

---

## 8. Net effect on PLAN.md

**Unchanged, and now evidenced:**
- One key per edge, no fan-in vectors — zot is the counter-example at v2 scale
  (§1.1). Harbor's `artifact_blob` / `artifact_reference` / `artifact_accessory`
  edge tables are independent confirmation.
- `commit_upload`, not `Move` — zot inherited `Move` unexamined and pays three
  full passes over every layer on S3 (§2.1).
- Resumable hashing in the session record — worth more than PLAN.md claims;
  it removes a step zot has not removed (§2.1).
- Blob storage holds bytes, not relationships — deletes zot's entire cache
  subsystem and their `hooks.go` rollback protocol (§2.3, §5).
- Offline purge for v1 — both zot (repo-wide lock) and Harbor (read-only mode,
  even with a full RDBMS and CAS) effectively do the same (§3).
- `MetaEngine` altitude is right; three-backend duplication is what the
  alternative costs (§1.3).

**Should change:**
- `F` gains a value (§6.1); cosign tag edges synthesised (§6.2).
- `P` gains `{size, added_at}` (§6.3).
- `ManifestRecord` gains `pushed_at`, `artifact_type`, `annotations`;
  `T` gains `tagged_at` (§6.1, §6.6).
- `UploadSession` gains hasher state + S3 multipart identifiers (§2.2, §6.7).
- A `DBVersion` key and migration hook (§6.9).
- Purge respects `U`-held `RepoId`s (§6.4).
- New risk in PLAN.md: **no rebuild path from storage** (§1.4, §6.9).
