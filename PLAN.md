# summ — build plan

An OCI Distribution Spec compliant container registry in Rust. One binary, no
dependencies, a built-in web UI, and a purpose-built metadata store that makes
discovery — what is in here, how much of it, and what references what — a
first-class operation rather than an afterthought.

This document is the entry point for a fresh session. It records what is decided,
what is built, and what is open. Update it as work lands.

## Why this exists

Four goals. They are not independent: the metadata store that makes discovery
fast is the same one that makes pulls fast, and the UI is what proves both are
real.

### 1. Extremely fast

Fast is the point, not a feature. Two things follow from the research, and they
are not what we assumed going in:

- **The byte path is nearly free.** Serving blob bytes well costs 2–5 % of an
  8-vCPU box at line rate, and the best possible implementation would save about
  1 % (`research/R2`). Throughput is not where the race is won.
- **Metadata latency is the product.** Four of the five serial steps in a cold
  containerd pull are metadata lookups, and being sequential their latencies add
  (`research/R5`). A registry that answers those in microseconds instead of
  milliseconds is a *visibly* faster registry, on exactly the path every pull
  takes.

So the speed goal and the discovery goal below are the same engineering problem,
approached from two directions.

### 2. First-class metadata discovery

The Distribution Spec is a transfer protocol. It can tell you the bytes of a
manifest you already know the name of; it cannot tell you what is in your
registry, how big it is, what a layer is shared by, or which manifests are
untagged and reclaimable. `GET /v2/_catalog` is not even in the spec — it was
removed before v1.0.0 and now sits in a reserved extension namespace, so
*nothing* standard answers the most basic question an operator has.

Every registry therefore bolts discovery on afterwards, over a store that was
designed for transfer, and it is slow: distribution walks the filesystem, zot
does a whole-repo read-modify-write, and the managed registries throttle or
time out. This is the concrete failure that started the project — **catalog and
list operations degrading at high repo counts, and provider rate limiting** on
ECR and ACR.

summ inverts the order. The metadata store is designed for discovery first, and
transfer is served from it. Listing repositories, listing tags, counting
manifests, summing a repo's size, and asking which manifests reference a layer
are all ordered prefix scans over an index built for exactly that — at ten
million repositories, with cursor pagination throughout and no operation that
materialises an unbounded set.

### 3. Simple to run — batteries included

One binary. No database to provision, no object store to configure before first
use, no sidecar, no migration step. `summ serve` on a laptop and on a
ten-million-repo host should be the same command. RocksDB is compiled in and
statically linked precisely so that "install the registry" is "copy one file".

This is a design constraint, not a convenience: every feature that would require
an external service has to justify itself against it, and the answer is usually
to build the capability in rather than depend out.

### 4. A built-in web UI

Shipped in the binary, served on the same port, no separate build or deploy.
Browse repositories, tags, manifests and their counts and sizes; drill into a
manifest; see what a layer is shared by.

The UI is not a side project — it is the reason goal 2 has a visible payoff, and
it is the honesty check on the extension API. If the UI can render a
ten-million-repo catalog responsively, the API underneath is genuinely
cursor-paged and genuinely fast. If it cannot, no amount of benchmark numbers
will cover for it. summdb already prototyped both the UI and the queries behind
it, so this is proven ground rather than speculation.

### Consequences

- The **extension API is core product surface**, not a nice-to-have. It is what
  the UI runs on and what makes summ worth choosing. It is also unstandardised,
  so it needs its own tests — nothing external will check it.
- **Everything is cursor-paged**, including the UI's own queries. A view that
  sorts or counts across the whole registry is a bug, however convenient.
- **Embedding the UI** means asset bundling in the binary and a UI that can be
  developed without a separate toolchain in the release path.
- Conformance remains the floor, not the ceiling: summ must pass the OCI suite,
  but passing it is not the goal.

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
| Web UI | Built in, served on the same port, assets embedded in the binary | No separate build or deploy. Constrains the extension API to be genuinely cursor-paged. |
| Extension API | Core product surface, versioned separately from `/v2/` | Unstandardised, so it carries its own test suite. |
| summdb | Prototype, not a dependency | Code copied in and reworked. summdb is not maintained once summ takes off. Its UI and stats queries are the starting point for Phase 2b. |

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

### Phase 2b — discovery API and web UI
The payoff for goals 2 and 4, and the honesty check on both. Cursor-paged
extension endpoints (see "Beyond the spec"), then the embedded UI on top of them.

Deliberately placed before the performance phase, not after. The UI is the thing
that will expose an unbounded scan or an accidental full-table sort, and it is
much cheaper to find those before tuning than after. If the UI can browse a
ten-million-repo catalog responsively, the API underneath is honest.

### Phase 3 — performance
Benchmark against distribution on the existing rig.

**R2 settled the blob-serving question, and the answer is "do the simple thing
well".** Measured on the bench host, cost of moving 1 GiB from page cache to
socket: a naive 4 KiB `ReaderStream` burns 11–15 % of an 8-vCPU box at line rate;
64 KiB chunks 5–11 %; **1 MiB chunks 2–5 %**; true `sendfile` 2–2.4 %. So the gap
between a badly-tuned copy loop and a well-tuned one is **3–5×**, and the gap
between a well-tuned loop and true zero-copy is **about 1 % of the machine** —
bought at the price of fighting hyper for the socket, breaking under TLS, and
blocking the reactor on page-cache misses. Bad trade.

**Do:** `pread` in `spawn_blocking`, **1 MiB chunks**, `Bytes` into hyper's
`writev`. **Do not:** `sendfile`, `mmap`, or an io_uring runtime. Tokio is
bringing io_uring in-tree transparently; revisit then, for free.

The real Phase 3 work is therefore metadata latency, not byte throughput — see
the client findings below.

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
| H | Extension API — cursor-paged discovery endpoints | `summ-server/` | E |
| I | Built-in web UI — embedded assets, served on the same port | `summ-ui/` | H |

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

### Sizing — measured

The one-key-per-edge rule trades O(N) write amplification for O(E) space, where E
is the number of reference edges. That is the right trade — space is cheap,
rewriting a 360 MB value per push is not — but E is large.

Measured on 20 M synthetic edge keys (`research/R3` §6): 79 B/key raw →
**53.12 B/key with no compression at all**, purely from RocksDB's block key
delta encoding, → **42.00 B/key** with zstd and 16 KiB blocks. Delta encoding
does more work here than compression does, which is what you would hope given
that edge keys share long digest prefixes.

**The dominant unknown is not a knob — it is the blob fan-out distribution.**
At the same 20 M keys, varying references per blob: 1 → 78.24 B/key (a 1 %
saving; delta encoding has nothing to bite on), 2 → 55.55, 10 → 42.00,
100 → 39.41. So aggregate `R` size is governed by how widely blobs are shared in
the real corpus, giving a planning range of roughly **800 GB to 1.6 TB**. That
spread is wider than every tuning option combined. See Risk 2.

One idea from R3 worth more than all the tuning: **interning manifest digests to
a `u32`, as repo names already are, would roughly halve the `R` range.** Not yet
adopted — it is a schema change with its own costs — but it is the largest single
lever available.

### Tuning — settled, see `research/R3`

Applied in `RocksEngine::open`. The headline: **RocksDB's default
`filter_policy` is `nullptr`** (`table.h:590`), so before this the engine had no
bloom filters at all.

- **Custom `SliceTransform` (`"summ.prefix.v1"`).** The prefix-consistency
  property RocksDB demands *is* satisfied by a variable-length transform,
  because the bytes deciding the length — the type byte, and for digest-bearing
  keys the algorithm byte — are themselves inside the prefix. Proved and verified
  experimentally. There are **six** prefix lengths, not the four first assumed:
  1, 5, 34, 38, 66, 70.
- **Measured: 118 829 → 735 796 negative `exists_prefix`/s, a 6.2× win** on the
  purge hot path, and the prefix filter costs **10× less space** than a
  whole-key filter (0.125 vs 1.25 B/key) because it holds one entry per group.
- **Whole-key blooms do nothing for `exists_prefix`** — it is a seek, not a
  point lookup. Kept anyway for `get`.
- **16 KiB blocks** shrink the index ~4× (a projected 11.2 GB → 2.9 GB) for the
  same data size. That, not compression, is the reason to raise it.
- **Explicit 512 MiB LRU block cache** — RocksDB's default is 32 MiB.
- **Rejected:** `optimize_for_point_lookup` (silently replaces the table factory
  and cache), `optimize_filters_for_hits` (drops exactly the filters purge
  needs), universal compaction, zstd dictionaries (measured *worse*), and
  column families. Cross-CF `WriteBatch` **is** atomic — verified by 12 rounds of
  SIGKILL during ~7000 three-CF batches, zero torn — so CFs were viable, but
  there is no merged cross-CF iterator and they leak a RocksDB concept into
  `MetaOp` and the replication log for nothing the transform does not already
  give.
- `auto_prefix_mode` is unusable: not exposed by the binding, and carries a
  documented `BUG:` in 11.8.1.

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
2. **Aggregate manifest count and blob fan-out are both unspecified**, and R3
   showed fan-out matters as much as count: per-key cost ranges from 78 B at one
   reference per blob to 39 B at a hundred, giving an ~800 GB to ~1.6 TB planning
   range for the `R` keyspace alone. "10M repos, 10M manifests per repo" bounds
   each axis but not the total. Assumed ~10⁹ manifests pending confirmation; a
   sample of the real corpus's sharing distribution would narrow this more than
   any further tuning.
3. **Filesystem fan-out.** distribution's 2 hex chars gives 256 directories; at
   10⁸ blobs that is ~400K entries each. Use three levels. See Blob storage.
4. **Offline purge is a scaling cliff.** Acceptable for v1 by decision. The
   upgrade path is upload-session pinning via `U` keys plus an mtime grace
   period — additive, not a redesign.

## What the client actually does (from `research/R5`)

Optimising for a synthetic benchmark is a way to win a benchmark and lose in
production. The findings that change the design:

- **Four of the five serial steps in a cold containerd pull are metadata**
  (`HEAD manifests/<tag>`, `GET manifests/<index>`, `GET manifests/<manifest>`,
  `GET blobs/<config>`), and they are strictly sequential, so their latencies
  add. **This is where RocksDB beats distribution's filesystem link walk** — and
  it means the `M`/`B`/`T` lookups deserve block cache, not just the `R` scans.
  Combined with R2: the byte path is nearly free, so *metadata latency is the
  product*.
- **`HEAD /manifests/<ref>` must be a first-class single-lookup endpoint** —
  `T` then `M`, two point lookups, no body read. Never implement it as "GET and
  discard the body".
- **containerd is HTTP/1.1 only**, pools 2 idle connections per host, and sizes
  at roughly 3–8 concurrent connections per pulling node. Do not invest in h2/h3;
  do keep TLS session resumption on and connection setup cheap.
- **Never return `429`.** containerd retries it immediately, five times,
  ignoring `Retry-After`. Since escaping provider rate limits is half the reason
  summ exists, the answer is to not need throttling at all; if it is ever added
  it must be a connection- or accept-level control, never a status code.
- **Never compress or transform blob bodies** — no `CompressionLayer` near
  `/blobs/`. The `B` key stores manifests zstd-compressed at rest, so decompress
  and serve `identity`: the digest is over the plaintext.
- **Design the blob path around aborted, open-ended range reads.** containerd
  2.1+ chunked fetch requests `bytes=N-`, reads 8 MiB, then kills the connection.
  Bottlerocket already ships this on by default. The test case is "client
  cancels 8 MiB into a 900 MB response": minimal wasted read-ahead, prompt fd
  release, no per-abort metric cardinality.
- **Abort, do not apologise.** On a mid-stream blob failure, tear the connection
  down. Appending anything converts a retryable short read into a digest
  mismatch.
- **The existing benchmark models the wrong shape.** `bench/loadtest` does
  `GET manifest → GET all blobs concurrently`. To be honest it needs the leading
  `HEAD`, the index→manifest→config→layers serialisation, and a warm-cache mode
  that skips already-held layers — otherwise summ gets tuned for pure throughput
  while real pulls are dominated by four serial metadata round trips.

## Dependencies

`oci-spec` 0.10.0 is the one clear win — take it rather than hand-rolling
manifest and descriptor types. `oci-client` 0.17.0 is worth having in dev
dependencies for tests and the bench harness, not in the server. Detail and the
rejected alternatives are in `research/R5`.

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
- **`DeletePrefix` is the wrong primitive for a single blob's edge range** (it
  is ~10 keys) — point-delete them instead, which also strengthens the case for
  `scan_keys`.
- **Consider interning manifest digests to a `u32`.** R3 measures this as
  roughly halving the `R` keyspace — a bigger space win than every tuning knob
  combined.
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
| ~~R2~~ | ~~Zero-copy blob serving~~ — **done**, `research/R2-zero-copy-serving.md`; answer: do not. `sendfile` via `spawn_blocking` vs `io_uring`/`tokio-uring` maturity vs plain `tokio::fs` streaming; plus Range handling. | Phase 3 | This is where "very fast" is won. `../container-registry/notes/fs_limit.md` shows large pulls are network-saturated at ~1.56 GB/s, so the goal is near-zero CPU per byte, spending the headroom on concurrency. |
| ~~R3~~ | ~~RocksDB tuning~~ — **done and applied**, `research/R3-rocksdb-tuning.md` | ~~Phase 2~~ | Prefix bloom filters are the biggest available win for `exists_prefix`, the purge hot path, but our prefixes come in four lengths (1, 5, 34, 66 bytes) and a fixed transform fits one. Column families would also let manifest bodies be tuned separately from small records. |
| ~~R4~~ | ~~zot prior art~~ — **done**, `research/R4-zot-prior-art.md` | ~~Packages C, E~~ | The closest prior art: a real registry with an embedded metadata store and a working S3 driver. Our schema is settled, so this is a cross-check for things we have missed — referrers handling, signature/attestation artifacts — and a second opinion on the driver trait. |
| ~~R5~~ | ~~Client behaviour + Rust ecosystem~~ — **done**, `research/R5-clients-and-ecosystem.md`. | Phase 3 | Determines what to optimise: request ordering, concurrency, whether it reuses connections, how it handles 206s. Optimising for a synthetic client is a way to win a benchmark and lose in production. |
| ~~R6~~ | ~~Rust ecosystem survey~~ — **done**, folded into `research/R5` Part B. | — | — |

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

## Beyond the spec — the extension API

`GET /v2/_catalog` is not in the Distribution Spec: removed before v1.0.0, the
path now sits in a reserved extension namespace, and the conformance suite never
exercises it. Verified — the string does not appear in `spec.md` at all.

That is liberating rather than awkward. The operation this project exists to make
fast carries no conformance obligation, so its pagination and ordering semantics
are ours to choose. The cost is that nothing external validates it, so the
extension API needs its own test suite.

Surface to build, all cursor-paged, all backed by prefix scans over the key
schema:

| Query | Backed by |
|---|---|
| List repositories | `n` (name-ordered) |
| List tags in a repo | `T <repo>` |
| List manifests in a repo, with counts | `M <repo>` |
| Repo size and manifest count | `P <repo>` |
| Which manifests reference this layer | `R <digest>` |
| Which tags point at this manifest | `G <repo> <digest>` |
| Untagged / reclaimable manifests | `M` minus `G` |

summdb prototyped several of these (`/v1/repos/:repo/stats`,
`/v1/layers/:digest/manifests`) along with the UI that consumes them; that code
is the starting point.
