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

**Built** (`cargo test` — 22 passing):

```
summ-core    digest (sha256/sha512), key encoding, value types, errors
summ-meta    MetaEngine trait, WriteBatch op log, redb engine, LRU repo interner
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

### Phase 0 — baseline
Run `distribution-spec/conformance` against local `distribution`. Capture a
perf baseline with the existing harness in `../container-registry/bench`
(Terraform + Ansible + Rust load tester, Azure and AWS). That harness is a major
asset: point it at summ from day one for a continuous A/B.

### Phase 1 — skeleton
axum, full route table, spec error model, config, single binary. Filesystem blob
store, naive metadata. **Exit: conformance push + pull pass.**

### Phase 2 — metadata engine
Wire `summ-meta` behind the HTTP layer. Catalog and tag pagination end-to-end.
**Must include a synthetic load at target scale before committing to redb** — see
Risks.

### Phase 3 — performance
Zero-copy blob serving, Range requests. Benchmark against distribution on the
existing rig. This is where "very fast" is won or lost.

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
| C | Filesystem blob store — content-addressed, multi-level fan-out | `summ-storage/` | — |
| D | Upload session handling — chunked PUT/PATCH, resumable offsets | `summ-server/`, `U` keys | B, C |
| E | Registry ops layer — manifest put/get/delete as `WriteBatch` builders | `summ-registry/` | — |
| F | Purge | `summ-purge/` | E |
| G | Scale benchmark — synthetic 10M-repo dataset, engine A/B | `benches/` | — |

Package E is the natural next step and the one most worth doing carefully: it is
where spec semantics meet the key schema.

## Engine choice (open)

**redb is the current implementation, not a decision.** It was the fastest way to
get a working store behind `MetaEngine`; the trait exists so swapping it is cheap.

### Sizing first

The one-key-per-edge rule trades O(N) write amplification for O(E) space, where E
is the number of reference edges. That is the right trade — space is cheap,
rewriting a 360 MB value per push is not — but E is large. At ~10⁹ manifests
referencing ~20 blobs each, the `R` range alone is ~2×10¹⁰ keys of ~70 bytes:
**order 1 TB before compression**. Edge keys are valueless and share long digest
prefixes, so they compress and prefix-compress well, but this has to be measured,
not assumed. Confirming aggregate manifest count (Risk 2) directly sizes this.

### What the workload actually looks like

- Reads: point lookups and short ordered prefix scans. Prefix-*existence* checks
  are the purge hot path.
- Writes: rare relative to reads, but each push inserts tens of **randomly
  distributed** keys (digest-prefixed, so effectively uniform).
- Deletes: bulk, during purge.

Random-key insert at volume is precisely where a B-tree is weakest and an LSM is
strongest.

### Leaning

| Engine | For | Against |
|---|---|---|
| **redb** (B-tree, pure Rust) | Zero friction, MVCC readers, good point-lookup latency, already wired | Random inserts into a multi-TB B-tree mean page splits and heavy write amplification; single writer serialises pushes |
| **RocksDB** (LSM, C++) | Turns random inserts into sequential writes; prefix bloom filters make `exists_prefix` cheap; block compression on highly-compressible edge keys; tombstone deletes suit purge; proven at TB scale | C++ toolchain, build time, tuning burden, compaction stalls |
| **fjall** (LSM, pure Rust) | LSM behaviour without the C++ dependency | Young; unproven at this scale |

**Current leaning: an LSM, most likely RocksDB, for the production target — and
redb retained as the zero-friction dev/test engine.** This is a leaning from the
access pattern, not a measurement. Package G settles it: synthetic 10⁸–10⁹-key
load, measuring insert throughput, space on disk after compaction, point-lookup
and `exists_prefix` latency, and bulk-delete cost.

Do not harden anything against redb specifics in the meantime.

## Replication and the WAL

Planned direction: a write-ahead log for metadata, shipped to replicas.

`WriteBatch` is already that log. It is a serialisable, self-contained
description of a change, and adding a WAL means persisting each batch to a log
alongside applying it — no change to callers.

The property that makes this work is that batches contain only `Put`, `Delete`,
and `DeletePrefix`. There is no read-modify-write anywhere, because the schema
has no fan-in vectors that would need one. **That makes every batch idempotent**,
so replay is safe, retries are safe, and a replica that reapplies an overlapping
suffix of the log converges. Had we kept the prototype's `merge` primitive,
replay would have required exactly-once delivery — a far harder problem.

Two constraints to preserve, both already in CLAUDE.md:

- No side-channel writes. Anything that bypasses `WriteBatch` is invisible to
  the log and will silently diverge replicas.
- No non-deterministic content in a batch. No timestamps generated at apply
  time, no random ids minted inside the engine — the caller supplies them, so
  the batch means the same thing wherever it is replayed.

Blobs need no WAL: they are content-addressed and immutable, so replication is
plain copy, or shared object storage.

## Risks

1. **The metadata engine is not chosen yet.** See "Engine choice" below. redb is
   the current implementation, not the decision. *Highest-risk unknown in the
   project.* Package G must settle it before Phase 2 hardens. Mitigation is
   already in place: nothing depends on redb beyond `MetaEngine`.
2. **Aggregate manifest count is still unspecified.** "10M repos, 10M manifests
   per repo" bounds each axis but not the total, and the total is what sizes the
   engine. Assumed ~10⁹ pending confirmation.
3. **Filesystem fan-out.** distribution uses 2 hex chars = 256 dirs. At 10⁸+
   blobs that is ~400K files per directory. Use 2–3 levels.
4. **Offline purge is a scaling cliff.** Acceptable for v1 by decision. The
   upgrade path is upload-session pinning via `U` keys plus an mtime grace
   period — additive, not a redesign.

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

Catalog/list pain is the reason this project exists, so summ should ship the OCI
surface *plus* an extension API for the cross-cutting queries the spec cannot
answer — per-repo stats, layer listings, reverse lookups. summdb prototyped these
(`/v1/repos/:repo/stats`, `/v1/layers/:digest/manifests`). This is the real
differentiator over distribution, more than raw speed.
