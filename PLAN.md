# summ — build plan

An OCI Distribution Spec compliant container registry in Rust. Single binary,
fast, with a purpose-built metadata store so catalog and tag listing stay cheap
at ten million repositories.

This document is the entry point for a fresh session. It records what is decided,
what is built, and what is open. Update it as work lands.

## Why this exists

Cloud registries (ECR, ACR) fall over on two axes at scale:

1. **Catalog and list operations** — slow or unusable at high repo counts.
2. **Rate limiting / throttling** — pull concurrency capped by the provider.

summ targets both directly. Everything else is secondary.

## Decisions locked

| Question | Decision | Consequence |
|---|---|---|
| Scale target | 10M repos; up to 10M manifests in a single repo | No API may materialise an unbounded set. Every list is cursor-paged. |
| Auth | None / static token for v1 | Auth is middleware, deferred to Phase 6. Not on the critical path. |
| Topology | Single node, but keep HA viable | All mutations flow through a serialisable `WriteBatch`. No engine types leak past the trait. |
| Workload | Full read-write, pull-optimised | Push must be correct and complete; perf work targets pull. |
| Metadata engine | **RocksDB**, compiled from source and statically linked | Single binary, no RocksDB install. redb retained as a second implementation to keep the trait honest. |
| Purge (GC) | Offline for v1 | Registry read-only during sweep. Schema already supports online later — see below. |
| Digest algorithms | sha256 + sha512 | Tagged enum, algorithm byte in key encoding. |
| Conformance bar | Core push/pull at Phase 1; referrers by Phase 6 | `distribution-spec/conformance` is the gate. |
| summdb | Prototype, not a dependency | Code copied in and reworked. summdb is not maintained once summ takes off. |

"GC" throughout means *registry* garbage collection — purging unreferenced blobs
and untagged manifests. Rust has no runtime GC; the terms are unrelated.

## The scale constraint that shapes everything

summdb (the prototype) stored fan-in relationships as vectors inside a single
value: `LayerRecord.manifests: Vec<ManifestRef>`, `ManifestRecord.tags:
Vec<String>`. That is fine at prototype scale and fatal at ours. A popular base
layer referenced by 10M manifests is a ~360 MB value, and it would be
read-modify-written inside a write transaction on **every push touching that
layer**.

**Rule: no stored value may grow with the size of the registry.** Fan-*out* data
(a manifest's own layers and children) is bounded and stays inline. Fan-*in* data
becomes one key per edge.

Three things fall out of this, and they are why the schema is shaped this way:

- Adding a reference is an O(1) insert instead of an O(N) rewrite.
- "Is this blob still referenced?" is a single seek on a prefix — which is what
  makes purge affordable.
- Read-modify-write disappears from the write path entirely. There is no `merge`
  primitive. That in turn makes every `WriteBatch` idempotent and replayable,
  which is the HA seam, obtained for free.

## Key schema

Single table, binary keys, postcard-encoded values. redb orders `&[u8]`
lexicographically, so prefix scans arrive pre-sorted in the order the spec's
pagination requires. Uppercase prefixes are data, lowercase are the interner.
Digests encode as one algorithm byte followed by raw hash bytes.

| Key | Value | Purpose |
|---|---|---|
| `M <repo> <digest>` | `ManifestRecord` | Manifest metadata |
| `B <repo> <digest>` | zstd(manifest JSON) | Manifest body as pushed, byte-exact |
| `T <repo> <tag>` | digest | Tag → manifest. Name-ordered: backs `tags/list` |
| `G <repo> <digest> <tag>` | — | Reverse of `T`. Empty scan ⇒ untagged ⇒ purgeable |
| `L <digest>` | `BlobRecord { size }` | Global blob metadata |
| `R <digest> <repo> <manifest>` | — | Blob → referencing manifests. **Purge hot path** |
| `P <repo> <digest>` | — | Repo's blob set, incl. uploaded-but-unreferenced |
| `S <repo> <child> <parent>` | — | Index → per-platform child edges |
| `F <repo> <subject> <referrer>` | — | OCI 1.1 referrers |
| `U <uuid>` | `UploadSession` | In-progress chunked upload |
| `n <name>` | repo id (BE u32) | Name-ordered. **`_catalog` pages over this** |
| `i <id>` | name | Reverse. `i <u32::MAX>` reserved as the id counter |

Two traps worth stating explicitly:

- **Page the catalog over `n`, never `i`.** `n` is name-ordered; `i` is
  insertion-ordered. The spec requires name order.
- **A blob is servable under a repo** only if `R <digest> <repo>` is non-empty
  or `P <repo> <digest>` exists. Do not serve a blob just because `L` exists —
  that leaks content across repos.

## Status

**Built** (`cargo test` — 36 passing):

```
summ-core    digest (sha256/sha512), key encoding, value types, errors
summ-meta    MetaEngine trait, WriteBatch op log, RocksDB engine (v1),
             redb engine (kept to keep the trait honest), LRU repo interner
```

Notable properties already covered by tests: cursor paging never exceeds its
limit and never strays across a repo boundary; `exists_prefix` correctly gates
purge; batches are atomic across every key a push touches; the interner stays
correct with its cache fully evicted (the 10M-repo case).

**Not built**: HTTP layer, blob storage, purge, conformance harness, everything
in Phases 1–6.

## Phases

Ordering rationale: the storage driver abstraction comes *after* a fast
filesystem driver exists, so the trait is shaped by measured requirements rather
than guesses. distribution's driver interface is the cautionary tale — its
Reader/Writer abstraction forces buffering that S3 does not need.

### Phase 0 — baseline — **done**, see `research/R1-spec-conformance.md`
Conformance harness recipe verified end to end against `distribution` v3.1.1,
with the exact commands in R1. Two gotchas worth not rediscovering: **on macOS
port 5000 is AirPlay Receiver** (it answers 403 with `Server: AirTunes`), so use
15000; and `storage.delete.enabled: true` must be set or the delete APIs skip.

**Do not treat distribution as a passing baseline.** It scores 743/91/16 against
the 1.1 certification profile, because it has no referrers route at all and no
sha512. On its honest feature set it is 511 pass / 0 fail. Calibrate against that,
not against the raw totals.

**Read the result vocabulary, not the FAIL count.** `errRegUnsupported` silently
downgrades FAIL to Skip (405 on delete, 202 on single-POST or mount), so a low
FAIL count can conceal an entire unimplemented API.

Perf baseline with `../container-registry/bench` is still to do.

### Phase 1 — skeleton
axum, full route table, spec error model, config, single binary. Filesystem blob
store, naive metadata. **Exit: conformance push + pull pass.**

Two things R1 moved *into* this phase that looked like later work:

- **Blob range serving is a Phase 1 correctness requirement, not a Phase 3
  optimisation.** The suite exercises six range cases with exact expected
  headers.
- **Write `F <repo> <subject> <referrer>` edges from Phase 1**, even though
  `/referrers/` stays 404 until Phase 6. Retrofitting them later costs a full
  manifest rescan plus a spec-mandated ingest of the fallback tag schema.

And one trap worth stating once: **there are two `Content-Range` grammars.**
Chunked *upload* uses a bare `0-1023` (`^[0-9]+-[0-9]+$`, no `bytes ` prefix, no
`/total`), and the `202` response echoes a bare `Range: 0-<end>`. Blob *download*
uses ordinary RFC 9110 `bytes 500-1499/<len>`. Out-of-order chunks MUST get
`416`.

### Phase 2 — metadata engine
Wire `summ-meta` behind the HTTP layer. Catalog and tag pagination end-to-end.
**Must include a synthetic load at target scale before committing to redb** — see
Risks.

### Phase 3 — performance
Zero-copy blob serving. Benchmark against distribution on the existing rig. This
is where "very fast" is won or lost. (Range *correctness* lands in Phase 1; what
belongs here is making it cheap.)

### Phase 4 — purge
Offline sweep exploiting `R` and `G`.

### Phase 5 — storage driver abstraction + S3
Chunked upload → S3 multipart.

### Phase 6 — referrers, auth, full conformance

## Work packages (parallelisable)

Independent enough to hand to separate subagents. Phase 1 packages can run
concurrently; each owns its files.

| # | Package | Owns | Depends on |
|---|---|---|---|
| A | Conformance harness — script to run the suite against a local binary, wired into CI | `conformance/` | — |
| B | HTTP skeleton — axum, routing, spec error codes, `Docker-Content-Digest` | `summ-server/` | — |
| C | Filesystem blob store — content-addressed, 3-level fan-out, `commit_upload` not `move` | `summ-storage/` | — |
| D | Upload session handling — chunked PUT/PATCH, resumable offsets | `summ-server/`, `U` keys | B, C |
| E | Registry ops layer — manifest put/get/delete as `WriteBatch` builders | `summ-registry/` | — |
| F | Purge | `summ-purge/` | E |
| G | Scale benchmark — synthetic 10M-repo dataset, engine A/B | `benches/` | — |

Package E is the natural next step and the one most worth doing carefully: it is
where spec semantics meet the key schema.

## Engine choice — RocksDB (decided)

**Decided for v1: RocksDB**, compiled from source by `librocksdb-sys` and
statically linked, so the registry ships as one binary with no RocksDB to
install. Verified: the only dynamic dependencies are OS-provided (`libc++`,
`libiconv`, `libSystem` on macOS; expect `libstdc++` + `libc` on Linux — a fully
static musl build is a separate exercise if wanted).

### Why an LSM

- Reads are point lookups and short ordered prefix scans; prefix-*existence*
  checks are the purge hot path.
- Writes are rare relative to reads, but each push inserts tens of **randomly
  distributed** keys (digest-prefixed, so effectively uniform).
- Purge does bulk deletes.

Random-key insert at volume is where a B-tree is weakest: page splits and write
amplification across a multi-terabyte tree. An LSM absorbs those into sequential
writes. Bulk deletes become cheap tombstones. And block compression matters more
here than usual, because most of the keyspace is valueless edge keys sharing long
digest prefixes.

### Sizing

The one-key-per-edge rule trades O(N) write amplification for O(E) space, where E
is the number of reference edges. That is the right trade — space is cheap,
rewriting a 360 MB value per push is not — but E is large. At ~10⁹ manifests
referencing ~20 blobs each, the `R` range alone is ~2×10¹⁰ keys of ~70 bytes:
**order 1 TB before compression**. Compression is therefore load-bearing, not a
nicety. Package G must measure post-compaction size on disk, not just throughput.
Confirming aggregate manifest count (Risk 2) directly sizes this.

### Still to tune

`RocksEngine::open` sets Lz4 with Zstd at the bottom level and nothing else.
Deliberately unturned until there are measurements:

- **Prefix bloom filters** would let `exists_prefix` skip SSTs entirely — the
  single biggest available win for purge. Blocked on the fact that our prefixes
  come in several lengths (1, 5, 34, 66 bytes) and a fixed-length prefix
  transform fits only one of them. Options: a custom `SliceTransform` that reads
  the type byte and returns the right length, or column families per key type.
- **Column families** to separate hot small records from cold manifest bodies.
- Block cache sizing, bloom bits/key, compaction style.

### redb

Kept as a second `MetaEngine` implementation, not as a fallback plan. The whole
integration suite runs against both, which is what keeps the trait honest and the
decision genuinely reversible. If it ever becomes a maintenance drag, delete it —
the tests are the asset, not the engine.

## Blob storage — what to take from distribution

**Take the content-addressed blob store. Reject the link structure.**

distribution's on-disk layout (`registry/storage/paths.go`) is two things fused
together: a content-addressed blob store, and a set of *link files* — tiny files
whose paths encode repo→blob membership, tag→manifest, and manifest revisions.

The link files exist because **distribution has no metadata database**. It encodes
relationships as filesystem paths so it can run against S3 alone. We have
RocksDB, where those relationships already live as `P`, `R`, `T`, and `G` keys.
Reproducing them on disk would mean two sources of truth that can silently
diverge — and it is precisely what makes distribution's GC a full storage-tree
walk and its catalog a recursive directory listing. On S3 those become LIST
storms. Slow catalog and list operations are the reason this project exists;
inheriting their cause would be self-defeating.

So blob storage becomes a pure content-addressed object store with **no directory
structure that carries meaning**: `digest → bytes`, nothing else. That keeps the
driver trait small, which is what makes an S3 driver clean later.

Four deliberate deviations:

1. **Deeper fan-out, no per-blob directory.** distribution uses
   `blobs/sha256/ab/<full-hex>/data` — 2 hex chars, so 256 first-level buckets,
   which at 10⁸ blobs is ~400K subdirectories each; and the per-blob directory
   doubles inode count to hold one file. Use `blobs/sha256/ab/cd/ef/<full-hex>`:
   three levels of two hex chars is 16.7M buckets, ~6 blobs per directory at 10⁸,
   and the file *is* the blob. On S3 the prefix depth is irrelevant, but keeping
   one layout across drivers costs nothing.

2. **Model `commit_upload`, not `move`.** distribution's driver trait exposes
   `Move`, which on S3 is a lie — there is no rename, so it degrades to
   copy-then-delete, and copying a multi-gigabyte layer to commit it is
   pathological. Instead the driver should expose "commit this upload as this
   digest" and implement it natively: the filesystem driver renames (atomic), the
   S3 driver completes a multipart upload straight at the final key. This is the
   single most important trait-design lesson to take from distribution, and it is
   a lesson by counter-example.

3. **Upload session state lives in RocksDB, not on disk.** distribution keeps
   `_uploads/<id>/startedat` as a file to expire abandoned uploads. We have
   `UploadSession` under `U` keys, which is cheaper to scan and already
   transactional with everything else.

4. **Resumable hashing does port.** distribution serialises hasher state to
   `hashstates/<algo>/<offset>` so a resumed chunked upload need not rehash from
   zero, relying on Go's `encoding.BinaryMarshaler`. Rust has an equivalent:
   `crypto-common` 0.2 exposes `hazmat::SerializableState`, and `sha2` 0.11
   implements it for both `Sha256VarCore` and `Sha512VarCore` (verified by
   experiment: state serialises to **104 bytes** for sha256, and a hasher
   rehydrated from it produces a digest identical to the uninterrupted one).

   This is better than distribution's arrangement, because 104 bytes fits
   directly in the `UploadSession` record under the `U` key rather than needing
   its own files on the storage driver. An interrupted chunked upload can then
   resume on any process, which removes what would otherwise have been an HA
   constraint. Note it requires `sha2` 0.11+; the trait does not exist in the
   0.10 line. The trait is in a `hazmat` module and the serialised state is
   sensitive - it must not be exposed outside the metadata store.

### Crash consistency

One ordering rule, and it is not negotiable: **blob bytes land and are fsynced
first; the metadata batch is the commit point.** A blob with no metadata is
harmless garbage that purge reclaims. Metadata referencing a missing blob is
corruption that surfaces as a failed pull. Never the reverse.


## Replication and the WAL

Planned direction: a write-ahead log for metadata, shipped to replicas.

`WriteBatch` is already that log. It is a serialisable, self-contained
description of a change, and adding a WAL means persisting each batch to a log
alongside applying it — no change to callers.

The property that makes this work is that batches contain only `Put`, `Delete`,
and `DeletePrefix`. There is no read-modify-write anywhere, because the schema
has no fan-in vectors that would need one. Had we kept the prototype's `merge`
primitive, replay would have required exactly-once delivery — a far harder
problem.

**One correction to an earlier, too-strong claim here.** `Put` and `Delete` are
self-contained: their effect is fully determined by the batch's own content.
`DeletePrefix` is not — its blast radius depends on what happens to be in the
store when it is applied. So a log of these batches is safe to replay **in
order, from a consistent point**, and replaying an overlapping suffix in order
converges. It is *not* safe under arbitrary reordering, nor against a store that
a second writer is mutating outside the log. Do not describe the batches as
simply "idempotent" without that qualifier; the WAL design must guarantee
ordered replay.

Two constraints to preserve, both already in CLAUDE.md:

- No side-channel writes. Anything that bypasses `WriteBatch` is invisible to
  the log and will silently diverge replicas.
- No non-deterministic content in a batch. No timestamps generated at apply
  time, no random ids minted inside the engine — the caller supplies them, so
  the batch means the same thing wherever it is replayed.

Blobs need no WAL: they are content-addressed and immutable, so replication is
plain copy, or shared object storage.

## Risks

0. **A lost or corrupt RocksDB is a dead registry.** This is the largest
   durability gap and it was previously unlisted. Manifest bytes live only under
   `B` and tags only under `T`, so the metadata store is authoritative with no
   way to reconstruct it. A full disk of blobs would be unidentifiable. zot does
   not have this problem — its metadata is a derived cache rebuilt by walking
   storage — which is also why its migration patch lists are empty: it can always
   rebuild. Two mitigations to decide on before v1 ships, in `research/R4`:
   write manifest bytes to the blob store as well as `B` (one small object per
   manifest, makes the corpus self-describing), and add a `DBVersion` key plus a
   migration hook now, because retrofitting a version marker onto a populated
   store is unpleasant. Neither recovers tags; the planned WAL is what covers
   those.

1. **RocksDB at 10¹⁰ keys is chosen but unmeasured.** The engine decision is
   made; the *tuning* is not, and post-compaction size on disk is the number that
   could still surprise us (see Sizing above). Package G measures it. Mitigation
   remains in place: nothing depends on RocksDB beyond `MetaEngine`, and a second
   implementation proves it.
2. **Aggregate manifest count is still unspecified.** "10M repos, 10M manifests
   per repo" bounds each axis but not the total, and the total is what sizes the
   engine. Assumed ~10⁹ pending confirmation.
3. **Filesystem fan-out.** distribution's 2 hex chars gives 256 directories; at
   10⁸ blobs that is ~400K entries each. Use three levels. See Blob storage.
4. **Offline purge is a scaling cliff.** Acceptable for v1 by decision. The
   upgrade path is upload-session pinning via `U` keys plus an mtime grace
   period — additive, not a redesign.

## Pending schema changes (from `research/R4`)

Found by reviewing zot and Harbor; agreed but **not yet applied**, deliberately
batched so the key schema changes once rather than three times. Apply together
once R1 (spec) and R3 (RocksDB tuning) have landed.

- **`F` must carry a value.** The referrers response is an image index whose
  entries require `artifactType` and `annotations`, and `?artifactType=`
  filters on them. `F` is valueless and `ManifestRecord` has neither field, so
  **a spec-compliant referrers response cannot be built from the current schema
  at all.** Give `F` a `ReferrerRecord { media_type, artifact_type, size,
  annotations }`. That is bounded fan-out — one referrer's own descriptor — so it
  does not violate the no-growing-values rule, and it turns the endpoint into a
  single ordered prefix scan with the filter applied during it. Harbor
  denormalises identically onto `artifact_accessory`; two independent systems
  landing on the same shape is a strong signal.
- **Synthesise `F` edges for legacy cosign tags** (`^sha256-<hex>\.(sig|sbom|att)$`).
  Otherwise deleting a subject manifest leaves the signature tag dangling
  forever with its layers pinned by `R`, and purge — which keys entirely off "is
  it tagged?" — never reclaims it. A leak, not a correctness bug, but silent.
- **`P` gains `{size, added_at}`** — the grace clock for expiring
  uploaded-but-unreferenced blobs has nowhere to live today.
- **`ManifestRecord` gains `pushed_at`, `artifact_type`, `annotations`; `T`
  gains `tagged_at`.** No timestamps anywhere means no retention story.
- **`UploadSession` gains the serialised hasher state** (104 bytes, see Blob
  storage) **and S3 multipart identifiers.**
- **Purge must treat any `RepoId` referenced by a live `U` session as live**,
  or it can retire an interner entry an in-flight upload still holds.
- **`MetaEngine` gains `scan_keys`** (or `Page` becomes value-optional) — purge
  scans millions of valueless edge keys and currently allocates an empty `Vec`
  per row.

Two rules to write down while they are fresh:

- **Never hold a repo-scoped lock across a request-body read.** zot's
  `imagestore.go:1039` is the bug report: an unread body under a write lock
  stalls everything until `ReadTimeout`. summ has no such lock today; the risk is
  introducing one for purge or upload-offset validation.
- **Never re-read a blob to verify it.** zot's S3 path costs three full passes
  over every layer — complete multipart, re-read the whole object to hash it,
  then `Move` (copy+delete). Hash on the way in; the resumable-hasher design
  already makes this free.

## Research status

Settled, with the finding recorded above: metadata engine (RocksDB), blob
storage layout, resumable upload hashing, online-vs-offline purge.

Still outstanding, most blocking first:

| # | Topic | Blocks | Why it is not obvious |
|---|---|---|---|
| ~~R1~~ | ~~Spec + conformance~~ — **done**, `research/R1-spec-conformance.md` | ~~Phase 1~~ | The endpoint list is the easy part. The sharp edges are the error-code taxonomy, `Docker-Content-Digest`, chunked-upload `Content-Range` validation and out-of-order rejection, cross-repo mount, pagination `Link` headers, content negotiation, and the referrers fallback tag schema. Guessing these means failing conformance late. |
| R2 | **Zero-copy blob serving on stable Rust, 2026.** `sendfile` via `spawn_blocking` vs `io_uring`/`tokio-uring` maturity vs plain `tokio::fs` streaming; plus Range handling. | Phase 3 | This is where "very fast" is won. `../container-registry/notes/fs_limit.md` shows large pulls are network-saturated at ~1.56 GB/s, so the goal is near-zero CPU per byte, spending the headroom on concurrency. |
| R3 | **RocksDB tuning: custom `SliceTransform` vs column families per key type.** | Phase 2 hardening | Prefix bloom filters are the biggest available win for `exists_prefix`, the purge hot path, but our prefixes come in four lengths (1, 5, 34, 66 bytes) and a fixed transform fits one. Column families would also let manifest bodies be tuned separately from small records. |
| ~~R4~~ | ~~zot prior art~~ — **done**, `research/R4-zot-prior-art.md` | ~~Packages C, E~~ | The closest prior art: a real registry with an embedded metadata store and a working S3 driver. Our schema is settled, so this is a cross-check for things we have missed — referrers handling, signature/attestation artifacts — and a second opinion on the driver trait. |
| R5 | **containerd's pull behaviour.** | Phase 3 | Determines what to optimise: request ordering, concurrency, whether it reuses connections, how it handles 206s. Optimising for a synthetic client is a way to win a benchmark and lose in production. |
| R6 | **Survey existing Rust registry implementations.** | — | Cheap, and worth one pass before committing to an HTTP/storage shape. |

Not research, but open and worth closing:

- **Aggregate manifest count** (Risk 2) — the per-axis bounds do not size the store.
- **No `LICENSE` file.** `Cargo.toml` declares Apache-2.0 but nothing backs it, so
  the repo is public with no effective licence grant.

## Reference material (sibling directories)

| Path | Use |
|---|---|
| `../distribution-spec` | Spec text + `conformance/` suite — the acceptance gate |
| `../distribution` | Reference impl. `registry/storage/paths.go` for the on-disk layout |
| `../zot` | Closest prior art: `pkg/meta/boltdb` (embedded metadata store), `pkg/storage/{types,s3,local}` |
| `../harbor` | Product-level features, not architecture |
| `../container-registry` | **Your** bench harness. `notes/fs_limit.md` has the capacity analysis |
| `../summdb` | The prototype this schema descends from |

From `notes/fs_limit.md`: on a Standard_L8s_v3 the ceiling is network
(~1.56 GB/s) before NVMe (~3 GB/s). Large-image pulls are network-saturated, so
the goal is to burn near-zero CPU per byte and spend the headroom on
concurrency. Note distribution's `maxthreads: 100` — precisely the kind of
artificial ceiling not to reproduce.

## Beyond the spec

**`GET /v2/_catalog` is not in the spec.** It was removed before v1.0.0 and the
path now sits in a reserved extension namespace; the conformance suite never
exercises it. Verified: the string does not appear in `spec.md` at all.

That is liberating rather than awkward. The single feature this project exists to
fix is not a conformance obligation, so its pagination and ordering semantics are
ours to choose — we are not bound to whatever `?n=`/`?last=` behaviour a
reference implementation happens to have. It does mean the catalog needs its own
tests, because nothing external will check it.

So summ ships the OCI surface *plus* an extension API for the cross-cutting
queries the spec cannot answer — catalog, per-repo stats, layer listings, reverse
lookups. summdb prototyped several (`/v1/repos/:repo/stats`,
`/v1/layers/:digest/manifests`). This is the real differentiator over
distribution, more than raw speed.
