# summ — build plan

`README.md` says what summ is and why anyone would want it. This document is
the entry point for a fresh session and is loaded into every one of them, so it
holds only what a session needs *before* starting work: decisions, schema,
status, invariants, and what is open. Working that has served its purpose lives
in `research/` and `design/`, linked from here.

## Why this exists

The goals are in the README; what they bind the code to is here.

- **Metadata latency is the product.** R2 measured the byte path at 2–5 % of an
  8-vCPU box at line rate, where a perfect implementation saves ~1 %, so
  throughput is not where the race is won; R5 found four of the five serial
  steps in a cold containerd pull are metadata lookups, and their latencies add.
- **Catalog and list operations degrading at high repo counts, plus ECR/ACR
  rate limiting, is the concrete failure that started this project.** The spec
  is a transfer protocol and cannot tell you what is in your registry, so every
  registry bolts discovery on over a store designed for transfer and it is slow.
  summ inverts the order: the store is designed for discovery, transfer served
  from it.
- **A feature needing an external service must justify itself against the
  single binary**, and the answer is usually to build it in.
- **The UI is the honesty check on discovery.** If it browses a
  ten-million-repo catalog responsively, the API is genuinely cursor-paged.
- **The extension API is core product surface and carries its own tests**,
  because nothing external validates it. Everything is cursor-paged, and
  conformance is the floor, not the ceiling.

## Decisions locked

| Question | Decision | Consequence |
|---|---|---|
| Scale target | 10M repos; up to 10M manifests in a single repo | No API may materialise an unbounded set. Every list is cursor-paged. |
| Auth | **Two static API keys, on one axis: `--auth none\|write\|all`** | One middleware over every surface. Anonymous read-write stays the default, so nothing on the critical path changed. |
| Topology | Single node, but keep HA viable | All mutations flow through a serialisable `WriteBatch`. No engine types leak past the trait. |
| Workload | Full read-write, pull-optimised | Push must be correct and complete; perf work targets pull. |
| Metadata engine | **RocksDB**, compiled from source and statically linked | Single binary, no RocksDB install. redb retained as a second implementation to keep the trait honest. |
| Purge (GC) | Offline for v1 | Registry read-only during sweep. Schema already supports online later — see below. |
| Digest algorithms | sha256 + sha512 | Tagged enum, algorithm byte in key encoding. |
| Conformance bar | Core push/pull at Phase 1; referrers by Phase 6 | `distribution-spec/conformance` is the gate. **Passing: 862 checks, zero failures, at `OCI_VERSION=1.1`.** Referrers landed early and passes with the rest. |
| Web UI | Built in, served on the same port, assets embedded in the binary | No separate build or deploy. Constrains the extension API to be genuinely cursor-paged. |
| Extension API | Core product surface, versioned separately from `/v2/` | Unstandardised, so it carries its own test suite. |
| summdb | Prototype, not a dependency | Code copied in and reworked. summdb is not maintained once summ takes off. Its UI and stats queries are the starting point for Phase 2b. |

"GC" throughout means *registry* garbage collection — purging unreferenced blobs
and untagged manifests. Rust has no runtime GC; the terms are unrelated.

## The scale constraint that shapes everything

summdb stored fan-in as vectors inside a value (`LayerRecord.manifests`,
`ManifestRecord.tags`). Fine at prototype scale, fatal at ours: a base layer
referenced by 10M manifests is a ~360 MB value, read-modify-written **on every
push touching that layer**. Hence the no-growing-values rule in CLAUDE.md —
fan-*out* is bounded and stays inline, fan-*in* becomes one key per edge.

Three things fall out, and they are why the schema is shaped this way:

- Adding a reference is an O(1) insert instead of an O(N) rewrite.
- "Is this blob still referenced?" is a single seek on a prefix — which is what
  makes purge affordable.
- Read-modify-write disappears from the write path entirely. There is no `merge`
  primitive, so every `WriteBatch` is replayable, which is the HA seam obtained
  for free.

## Key schema

Single table, binary keys, postcard-encoded values. redb orders `&[u8]`
lexicographically, so prefix scans arrive pre-sorted in the order the spec's
pagination requires. Uppercase prefixes are data, lowercase are the interner.
Digests encode as one algorithm byte followed by raw hash bytes.

| Key | Value | Purpose |
|---|---|---|
| `M <repo> <digest>` | `ManifestRecord` | Manifest metadata |
| `B <repo> <digest>` | zstd(manifest JSON) | Manifest body as pushed, byte-exact |
| `T <repo> <tag>` | `TagRecord { digest, tagged_at }` | Tag → manifest. Name-ordered: backs `tags/list` |
| `G <repo> <digest> <tag>` | — | Reverse of `T`. Empty scan ⇒ untagged ⇒ purgeable |
| `L <digest>` | `BlobRecord { size }` | Global blob metadata |
| `R <digest> <repo> <manifest>` | — | Blob → referencing manifests. **Purge hot path** |
| `P <repo> <digest>` | `RepoBlobRecord { size, added_at }` | Repo's blob set, incl. uploaded-but-unreferenced |
| `S <repo> <child> <parent>` | — | Index → per-platform child edges |
| `F <repo> <subject> <referrer>` | `ReferrerRecord` | OCI 1.1 referrers. Descriptor denormalised — the response cannot be built without it |
| `U <uuid>` | `UploadSession` | In-progress chunked upload |
| `n <name>` | repo id (BE u32) | Name-ordered. **`_catalog` pages over this** |
| `i <id>` | name | Reverse. `i <u32::MAX>` reserved as the id counter |
| `v` | schema version (BE u32) | Single key. Absent on a populated store ⇒ refuse to open |
| `H <repo> <tag> 0 <!ms> <digest>` | `TagEvent` | Tag history, newest first. **Served** |
| `J <repo> <digest> <!ms> <tag>` | `TagEvent` | Same, addressed by digest. **Served** |
| `A <scope> <…> <day> <shard>` | `CounterBucket` | Pull counters, per day and per hour. **Served** |

`H` and `J` are complete — written on every tag mutation since Phase 1, and now
read by `/api/v1/tag-history/` and the UI. See **Tag history** below. `A` is
complete too: written by the flush task, read by `/api/v1/pull-counts/` and the
UI. See **Pull counts**.

**`H` and `J` timestamps are milliseconds; every stored record is seconds.**
That asymmetry is deliberate and it is the one thing to know before touching
either range. `TagRecord.tagged_at`, `ManifestRecord.pushed_at` and
`RepoBlobRecord.added_at` are a second's resolution and always have been. Tag
events cannot be: `H <repo> <tag> 0 <!ts> <digest>` puts the digest in the key
as the collision breaker, so a create and a delete of one tag at one digest
inside the same second encode to **the same key** and the later write silently
replaces the earlier — a hole in an audit trail. Two distinct events in one
second come back ordered by digest rather than by what happened. Deleting and
re-pushing a tag from a script does exactly this. `summ_core::Timestamp` carries
milliseconds and hands out seconds on request, so a caller has to say which it
wants and the compiler asks. The two encodings are self-distinguishing — a
seconds value now is ~1.7e9, a milliseconds value ~1.7e12, and they cannot
overlap until 5138 — which is why the change cost a populated store nothing:
old events read back correctly by magnitude, and because milliseconds are
larger every new event still sorts as newer than every old one.

Two traps worth stating explicitly:

- **Page the catalog over `n`, never `i`.** `n` is name-ordered; `i` is
  insertion-ordered. The spec requires name order.
- **A blob is servable under a repo** only if `R <digest> <repo>` is non-empty
  or `P <repo> <digest>` exists. Do not serve a blob just because `L` exists —
  that leaks content across repos.
- **No `skip_serializing_if` on a stored record, ever.** postcard is not
  self-describing, so a skipped field is not "absent" on the wire — it is
  missing, and the decoder reads the *next* field's bytes. `Platform::variant`
  carried it from the initial commit, which meant every ordinary multi-arch
  index push wrote an `M` record that could not be read back. Fixed, and
  `summ-core/tests/postcard_roundtrip.rs` now encodes and decodes every stored
  record so it cannot come back.

## Status

`cargo test` — **428 passing**. Every crate in CLAUDE.md's Layout is built, and
**the wiring has landed** (package K, `summ-server/src/backend.rs`): `summ
serve` runs on `summ-registry` over `summ-meta` with `summ-storage` holding the
bytes. `tests/wiring.rs` drives the same router as `tests/api.rs` against a real
store, and the tests that matter reopen it — a registry that loses a push on
restart passes every test in `api.rs`.

Verified end to end, by test and by hand: push and pull, byte-exact manifest
return, tags and catalog, ranges including containerd's open-ended `bytes=N-`,
cross-repo mount, per-repo blob isolation, delete cascade, and all of it
surviving a restart. A chunked upload **resumes across a process restart** —
offset and the 104-byte hasher state come back from the `U` record and the
commit still verifies. `oras cp` has pushed a real multi-arch `nginx:latest`
with reference validation on, and it read back byte-exact.

**Both directions stream.** Pushing a 200 MB blob moved release-binary RSS from
15.6 MB to 15.8 MB; serving it back took it to 20 MB.

**A real large image goes through end to end, and there is a script for it.**
`scripts/large-image-e2e.sh` builds the release binary, starts it on a scratch
data directory, pushes an image, then pulls every byte back from N clients at
once with each blob's digest verified — plus the two range shapes containerd
sends, the discovery API, and a restart. Measured on
`pytorch/pytorch:2.9.0-cuda12.8-cudnn9-runtime`, whose single largest layer is
**7.42 GiB**: push 7.8 s (~1.0 GB/s), four concurrent verified pulls of 7.52 GiB
each in 28.5 s (~1.1 GB/s aggregate, and shasum-bound rather than
registry-bound), zero mismatches, manifest byte-exact across the restart. That
layer is what the 1 GiB ceiling used to reject, so the script is also the
regression test for it. Image bytes come from the local docker daemon
(`docker save` writes an OCI layout that `oras cp` pushes straight from) or from
the upstream registry; nothing goes through `docker push`, because on Docker
Desktop `127.0.0.1` inside the VM is not the host.

**The per-request upload ceiling is 32 GiB, and it is a guard rather than a
wall** — `--max-upload-bytes` (`SUMM_MAX_UPLOAD_BYTES`), `0` to remove it. It
was 1 GiB and unreachable from the command line, left from the skeleton that
buffered a chunk in memory. **No client chunks a layer** — docker, crane and
oras all send one monolithic body — so that number was not a per-request bound
at all, it was the largest layer the registry accepted, and a `pytorch/pytorch`
push died on it with `413 SIZE_INVALID` that no retry could fix. The body
streams to the staging file, so the ceiling now costs disk rather than memory,
and a declared `Content-Length` above it is refused before the first frame
instead of after a ceiling's worth of bytes has been written.

**Every manifest is also in the blob store**, under its own digest, written and
fsynced before the push's batch commits. `B` is still the read path; the copy
exists so a disk of blobs is self-describing rather than unidentifiable content.
See Risk 0, which carries the rule purge has to honour.

Properties the tests pin down: cursor paging never exceeds its limit nor strays
across a repo boundary; `exists_prefix` correctly gates purge; batches are
atomic across every key a push touches; the interner stays correct with its
cache fully evicted (the 10M-repo case); prefix groups stay contiguous across
every in-domain range, which is what `prefix_same_as_start` relies on; every
stored record survives a postcard round trip; all four upload flows, all six
blob range cases, both `Content-Range` grammars.

**The discovery API and the web UI have landed** (packages H and I, first cut).
`/api/v1/` serves four cursor-paged read-only endpoints and `/` serves a
built-in UI over them. Assets are `include_str!`d, so `cargo build` is the whole
pipeline and the page loads nothing from the network. Verified by hand against a
real corpus: `oras cp` of alpine, busybox, nginx, postgres and redis, browsed
end to end.

**The referrers API is on** (end-12a/12b), which is the one part of Phase 6
that landed early. The `F` edges have been written since Phase 1, so the work
was pagination and a switch, not an index: `?n=`/`?last=` over the edge range
with `Link: rel="next"`, `?artifactType=` filtered inside the scan and claimed
through `OCI-Filters-Applied`, and `--no-referrers` to turn the endpoint off
again. Two rules are load-bearing and easy to regress — see **The referrers
API** below.

**summ passes the conformance suite at every profile**, per R1's recipe:

| Run | Checks | FAIL | Not passing |
|---|---|---|---|
| `OCI_VERSION=1.1` — the certification profile | 862 | 0 | 5 rows `Disabled` by the suite's defaults |
| `OCI_VERSION=dev` — R1's stretch target | 986 | 0 | *Sparse Manifests*, off by default |
| `dev` + `OCI_DATA_SPARSE=true`, server on `--allow-missing-references` | **1032** | **0** | nothing — every row `Pass` |

All three exit 0, and nothing is ever `Skip`, so no row is hiding an
unimplemented API behind `errRegUnsupported`. `dev` is the harder profile: it
additionally demands `Docker-Content-Digest` on blob *and* manifest responses,
requires upload-cancel, and turns on `?tag=` params for manifest PUT. For scale,
distribution v3.1.1 scores 743/91 at 1.1 and 826/143 at `dev`.

*Sparse Manifests* needs `--allow-missing-references` because it pushes
manifests and layers concurrently. That stays off by default: validation is
optional in the spec and costs N lookups on every push.

**Both engines pass.** The 1.1 run against `--engine redb` is also 862/0, which
is the cheapest available proof that nothing has leaked past `MetaEngine`.

**None of this is a gate yet.** Every run above was driven by hand, so nothing
stops the next commit regressing it — that is package A. And conformance is the
floor: it says nothing about the discovery API, the UI, auth or purge.

Two bugs stood between the first run and this one, both found by the suite and
both fixed:

- **sha512 could not be pushed without the algorithm hint.** An upload session
  chose its hasher at `POST` time and never revisited it, so a client that
  opened a plain session and closed it with `?digest=sha512:…` got `400
  DIGEST_INVALID` on content that had hashed perfectly well — under the other
  algorithm. `?digest-algorithm=` is a SHOULD (end-4c) and *no* client in the
  suite sends it: every blob flow carries a literal `// TODO: add digest
  algorithm if not sha256`. Fixed by rehashing — see **Digest algorithms**.
- **A manifest with non-distributable layers was rejected.** Reference
  validation demanded a blob for every descriptor, including the ones carrying
  `urls`, which name content hosted elsewhere that a registry is not expected to
  hold. One data row failing cascaded into six: *Non-distributable Layers* plus
  the *Manifest put/get/head by digest and by tag* rows and *Tag listing*, all of
  which use that data. See **Foreign layers**.

**Tag history is served** (`/api/v1/tag-history/<name>@<reference>`, plus a
timeline in the UI). The events needed no new writer: `H` and `J` have been
written on every tag mutation since Phase 1, inside the same `WriteBatch` as
the mutation, so this was a read path, an endpoint and a page. Two things did
change on the way — the timestamps moved from seconds to milliseconds, and the
`before` cursor became strictly-before, which it was not. See **Tag history**.

**Pull counts are served** (`/api/v1/pull-counts/<name>[@<reference>]`, plus
two contribution grids in the UI). This was package J, and it needed no new
engine primitive: a pull adds to a map in memory and returns, and a background
task folds the map into the `A` range every five seconds as plain `Put`s of
absolute values. The `CounterBucket` value gained a per-hour breakdown on the
way in, which cost nothing because nothing had ever written an `A` key. On by
default; `--no-pull-counts` turns it off. See **Pull counts**.

**API-key authentication has landed**, described under **Authentication**
below. Off by default, so the conformance runs above and every measurement in
this document are unaffected.

**There is a downloadable Linux x86_64 binary** (`.github/workflows/release.yml`).
A tag publishes a release; a manual dispatch re-points a rolling `dev`
prerelease at that commit, which is the trigger summ-bench wants — it measures
the working tree, so the commit it needs a binary for is never a release, and
compiling RocksDB on the benchmark VM costs more than the benchmark does.

**It is glibc, not musl, and that is a measurement decision.** musl's allocator
is markedly slower under the multi-threaded allocation churn RocksDB generates,
so a static musl summ would be a benchmark of musl. The build instead links
libstdc++ in — which `-static-libstdc++` cannot do, because librocksdb-sys emits
an explicit `cargo:rustc-link-lib=dylib=stdc++` that the flag has no opinion
about, so the fix is an `-L` directory holding `libstdc++.a` and no `.so`. What
is left is `libc`, `libm` and `libgcc_s`, which are the OS's ABI rather than
summ's dependencies; std links `gcc_s` itself unless the target is crt-static,
and removing it would mean overriding the unwinder std chose. Measured floor:
**GLIBC_2.34**; the README turns that into a distro list. The job asserts the
whole `ldd` set against an allowlist and starts the binary before packaging it.

**Not built**: purge, the conformance run in CI, Phases 3–6, and the rest of
the discovery surface (blob fan-in and the untagged set — both listed under
**Beyond the spec**).

## Phases

The storage driver abstraction comes *after* a fast filesystem driver exists, so
the trait is shaped by measured requirements. distribution's Reader/Writer
abstraction, which forces buffering S3 does not need, is the counter-example.

### Phase 0 — baseline — **done**, `research/R1-spec-conformance.md`

Harness recipe verified against `distribution` v3.1.1; exact commands in R1.
Three things not to rediscover:

- **On macOS port 5000 is AirPlay Receiver.** It holds `*:5000` on both address
  families and answers 403 with `Server: AirTunes`, so a summ bound to
  `127.0.0.1` never sees a request to `localhost`. Use 15000.
- **distribution is not a passing baseline.** Its 743/91/16 against the 1.1
  profile reflects having no referrers route and no sha512. On its honest
  feature set it is 511 pass / 0 fail.
- **Read the result vocabulary, not the FAIL count.** `errRegUnsupported`
  downgrades FAIL to Skip, so a low FAIL count can hide an unimplemented API.

Perf baseline with `../summ-bench/bench` still to do.

### Phase 1 — skeleton — **done**

The exit criterion is met: the suite passes clean at `OCI_VERSION=1.1`. Package
A remains as *automation* — the run above was driven by hand, and it belongs in
CI before it can be called a gate rather than an observation. It will need:

- `--allow-missing-references`. Reference validation defaults on; R1 recommends
  against it because it is optional, costs N lookups, and breaks a client
  pushing layers and manifest concurrently — which is what `OCI_DATA_SPARSE`
  does. On, a manifest naming an absent blob is `400 MANIFEST_BLOB_UNKNOWN`, its
  own code rather than `MANIFEST_INVALID`, because the document is well-formed
  and the fix is to push the blob.
- `--engine redb`, running the whole binary on the second `MetaEngine` — the
  cheapest proof nothing has leaked past the trait.
- `--listen '[::]:15000'`, per the AirPlay note.

Two things R1 moved *into* this phase: **blob range serving is a correctness
requirement**, not a Phase 3 optimisation; and **write `F` edges from Phase 1**
even though `/referrers/` was 404 at the time, because retrofitting them costs
a full manifest rescan plus a spec-mandated ingest of the fallback tag schema.
That call paid off: enabling the endpoint later cost a page and a switch, with
no rescan. See **The referrers API**.

**There are two `Content-Range` grammars.** Chunked *upload* uses a bare
`0-1023` (`^[0-9]+-[0-9]+$`, no `bytes ` prefix, no `/total`) and the `202`
echoes a bare `Range: 0-<end>`. Blob *download* uses RFC 9110
`bytes 500-1499/<len>`. Out-of-order chunks MUST get `416`.

### Phase 2 — metadata engine

Largely subsumed by package K. **Must include a synthetic load at target scale**
— see Risks.

### Phase 2b — discovery API and web UI — **first cut landed**

Cursor-paged extension endpoints (see **Beyond the spec**), then the UI on them.
Deliberately before the performance phase: the UI is what exposes an unbounded
scan or an accidental full-table sort, and those are cheaper to find before
tuning than after.

Built: `/api/v1/repositories`, `/api/v1/repositories/<name>`,
`/api/v1/tags/<name>`, `/api/v1/manifests/<name>` and
`/api/v1/manifests/<name>@<reference>`, and the UI over them. Four decisions
that are now contract:

- **The route table is flat, and it has to be.** A nested
  `/repositories/<name>/tags` is ambiguous when a registry holds both `foo` and
  `foo/tags`, and the wrong resolution does not 404 — it silently answers with
  the other repository's data. Each collection is its own top-level resource and
  the name runs to the end of the path. `/v2/` lives with the ambiguity because
  its shapes are fixed by the spec; this API is ours, so it is built out of it.
- **Counts are bounded and say so.** There is no stored total — keeping one
  would be the read-modify-write on the push path the schema exists to avoid —
  so a count folds pages to a ceiling (`seam::COUNT_CEILING`, 10,000) and
  carries `complete`. A UI renders `complete: false` as `10,000+`.
- **Search is a name prefix, not a substring.** `n <name>` is the name appended
  to one type byte, so a name prefix *is* a key prefix: `?q=` narrows the scan
  to one seek and a walk of the matching run. A substring search would be a pass
  over the catalogue and is deliberately not offered.
- **These reads go through `spawn_blocking`.** They are the exception to the
  inline-read bet below: a page of summaries folds a bounded count per row, so
  it is milliseconds of CPU rather than the microseconds a point lookup costs.

The UI lives in `summ-server/ui/` and `summ-server/src/ui.rs`, not the
`summ-ui/` crate the work-package table names. With no build step there is
nothing for a crate to own but four `include_str!`s; give it one when there is
an asset pipeline to put in it.

**Tag history and pull counts have since landed** —
`/api/v1/tag-history/<name>@<reference>` and `/api/v1/pull-counts/<name>` with
the UI over both. Still to build here: blob fan-in ("what shares this layer")
and the untagged / reclaimable set — the schema and the ops-layer queries exist
for both.

### Phase 3 — performance

**R2 settled blob serving: do the simple thing well.** Moving 1 GiB from page
cache to socket costs 11–15 % of an 8-vCPU box with a naive 4 KiB
`ReaderStream`, 5–11 % at 64 KiB, **2–5 % at 1 MiB**, 2–2.4 % for true
`sendfile`. So a bad copy loop costs 3–5× a good one, and zero-copy buys ~1 % of
the machine at the price of fighting hyper for the socket, breaking under TLS,
and blocking the reactor on page-cache misses.

**Do:** `pread` in `spawn_blocking`, 1 MiB chunks, `Bytes` into hyper's
`writev`. **Do not:** `sendfile`, `mmap`, or an io_uring runtime — Tokio is
bringing io_uring in-tree transparently.

The real work here is metadata latency, not throughput. See **What the client
actually does**.

### Phases 4–6

**4** offline purge exploiting `R` and `G` — and retaining every blob whose
digest is a manifest digest, or it destroys Risk 0's mitigation on its first
run. **5** storage driver abstraction +
S3, chunked upload → multipart. **6** full conformance — of which **referrers**
and now **auth** both landed early: the `F` edges had been written since Phase 1,
so referrers was a page and a switch, and API keys turned out to be one
middleware rather than an architecture. What is left of Phase 6 is anything
beyond a shared secret — per-repository scopes, a token endpoint, users.

## The referrers API

Served, on by default, and passing conformance. Four things decided here that
the next change to this endpoint must not undo:

- **`Link` is driven by the cursor, never by the page being full.** The
  `?artifactType=` filter is applied *inside* the scan and the cursor advances
  over the edges scanned, so a page can come back short — even empty — with
  matches still ahead. Emitting `Link` only on a full page would end the walk on
  the first sparse page and report that a rare artifact type has no referrers.
  The alternative, refilling until the page is full, makes one request scan an
  unbounded number of edges, which is the one shape this design forbids.
- **The `Link` carries `artifactType` through.** A link that drops the filter
  points at a different query, and the reply arrives without
  `OCI-Filters-Applied` — so it looks authoritative while being a page of
  something else.
- **`OCI-Subject` is gated on the endpoint being served.** The header means
  "this registry processed your subject", which is only true for a registry that
  will list it. Sending it while `/referrers/` answers `404` tells a client both
  that the tag-schema fallback is unnecessary and, one request later, that it is
  required.
- **`artifactType` is resolved on the push path, not the read path.** An image
  manifest with no `artifactType` reports its *config descriptor's* `mediaType`;
  an index with none reports nothing at all. `ManifestParse.referrer_artifact_type`
  holds the effective value and it lands on the `F` edge, which is what keeps
  the endpoint a pure ordered scan rather than a scan plus a manifest re-parse
  per referrer.

**Not implemented, deliberately: ingest of the referrers tag schema.** The spec
asks a registry enabling the API to pick up manifests recorded in an index
tagged `<alg>-<hex>` (§Enabling the Referrers API). That exists for registries
that accepted subject-bearing manifests while not indexing them; summ has
written `F` edges since the first push, so the window never existed. A repo
copied in from a fallback-tag registry arrives as manifests carrying real
`subject` fields, and those write their edges on the way in — the fallback index
is a redundant tagged copy. The only content this would recover is a fallback
index naming a manifest that has no `subject` of its own. Nothing in the
conformance suite covers it.

Legacy cosign tags are the backward-compatibility case that *does* matter in the
wild, and they are handled: `sha256-<hex>.sig|.sbom|.att` synthesises its `F`
edge at tag time and retracts it when the tag moves (`summ-registry/src/cosign.rs`).
That is a purge fix as much as a discovery one — see the module docs.

## Tag history

Served at `GET /api/v1/tag-history/<name>@<reference>`, newest first, with the
UI showing a timeline on the manifest page and a per-tag page behind every tag
chip. `<reference>` is a tag **or** a digest and the two are different
questions — "what has this name pointed at" reads `H`, "what has this manifest
been called" reads `J` — which is why one route serves both. Five decisions
here, and each is a thing a change could quietly undo:

- **It is an event log, not a list of what a tag pointed to.** A repoint writes
  one `Created` for the new digest and *no* `Deleted` for the displaced one, so
  "what did this tag point to at time T" is a fold: the newest `Created` at or
  before T, unless a `Deleted` is newer. And the event is written
  unconditionally — `previous` is consulted only to retract the old `G` edge —
  so re-pushing a tag at the digest it already has is a second row. It records
  **pushes, not changes**, which is right for an audit trail and is why a CI job
  pushing hourly accumulates ~8,800 rows per tag per year with nothing trimming
  them. Retention is the open item below.

- **Timestamps are milliseconds, and that is not a detail.** At a second's
  resolution a create and a delete of one tag at one digest inside the same
  second encode to the same key, and the later `Put` silently replaces the
  earlier. See the note under **Key schema** for the full argument and why the
  migration was free.

- **`before` is strictly-before, and the page cursor is a pair.** Seeking to
  `!ts` is *inclusive*, because an event at exactly `ts` is longer than the
  cursor and therefore sorts after it; the builder backs up one millisecond.
  And a timestamp alone is not a sufficient cursor: the key's tiebreaker is the
  digest (`H`) or the tag (`J`), so a page can end inside an instant and
  resuming from the instant alone would skip the rest of it. `next` carries
  `{before, last}`, both of them values off the last row — the house pagination
  style, not an opaque token. `before` on its own remains a real filter, which
  is what distribution-spec#606 means by it.

- **History outlives what it describes, so nothing here 404s.** An unknown
  repository, tag or manifest is an **empty page**. After a tag is deleted there
  is nothing left to tell "never existed" from "gone", and the endpoint must
  answer for the second case. The denormalised `media_type` and `size` in
  `TagEvent` exist for the same reason: every `Deleted` row describes a manifest
  `M` may no longer hold, and a row still has to render. The UI does not link a
  deleted row anywhere.

- **The manifest-delete cascade writes events too.** `delete.rs` routes through
  `stage_delete_tag` for every tag it drops, and it does so *before* removing
  the manifest, so the events still capture the descriptor.

**Not built: the spec-shaped endpoint.** distribution-spec#606 proposes
`GET /v2/<name>/_oci/tag-history/<reference>` returning an OCI image index whose
entries carry `.tag.timestamp` and `.tag.event` annotations. It is unmerged and
its data model is still argued over, so committing to that envelope now means
owning a guess. The rows are already in its vocabulary (`created` / `deleted`),
so if it merges the work is a response adapter over the same scan.

**Open: retention**, shared with pull counts. `A`, `H` and `J` are the only
ranges bounded by time rather than by current state, and nothing trims them. A window is a `DeletePrefix` over
a bounded suffix of each range — the keys are ordered newest-first, so "older
than X" is a contiguous tail — and it belongs to the purge sweep (package F),
which already walks the store. #606 permits trimming history when the repo is
deleted; a time window is our own policy and needs a flag.

## Authentication

Two static API keys — a read key and a write key — behind one flag with three
values. `--auth` (`SUMM_AUTH`) answers exactly one question, *which requests
require a key*, and its values are the three answers:

| `--auth` | reads | writes |
|---|---|---|
| `none` (default) | open | open |
| `write` | open | write key |
| `all` | read key | write key |

Naming them for what needs a key rather than for how the key travels is the
point: `apikey` would have been a *mechanism* sitting in a *scope* enum, so a
second mechanism later — htpasswd, OIDC — would have had to become a fourth
value that also silently redecided the scope. It stays a separate choice.

A supplied key comes from `--read-apikey` / `--write-apikey` or
`SUMM_READ_APIKEY` / `SUMM_WRITE_APIKEY`; an absent one is generated and
printed once in the startup banner. Six decisions here, and each of them is a
thing a change could quietly undo:

- **The challenge is `Basic`, not `Bearer`.** `Bearer` is what the hosted
  registries advertise, and it means something specific to a client: the
  challenge's `realm` is a *token server*, and docker and containerd will `GET`
  it, exchange credentials for a scoped token, and only then retry. Advertising
  `Bearer` without standing one up produces a client that fails in the token
  exchange rather than one that authenticates. `Basic` is the challenge those
  same clients answer with the credential they already hold, which is exactly
  the model when the key *is* the credential — so `docker login`, `oras login`
  and a browser opening the UI all work with no token endpoint existing.
  `Bearer <key>` is *accepted* anyway, for `curl`, and never advertised.
- **Under `all`, everything is behind it — `/v2/`, `/api/v1/` and the UI —
  through one middleware, and there is no exemption list.** An exemption is a
  hole that has to be re-argued on every new route, and both candidates fail on
  inspection. `GET /v2/` is the endpoint whose `401` is how a client
  *discovers* it needs credentials, so exempting it breaks the flow it exists
  for; and serving the UI shell anonymously only moves the prompt to the first
  `fetch`, where a native browser dialog on an XHR is strictly worse.
- **`write` splits by method and by nothing else.** The rule is `Access::of` —
  `GET`, `HEAD` and `OPTIONS` read, everything else writes, an unknown method
  writes — applied to every route alike, so the middle mode is a statement
  about what a request *does* rather than the route list an exemption would
  have been. Its consequence is the mode rather than a hole in it, and the
  banner says so: `_catalog`, every tag list, every manifest and the whole UI
  are readable by anyone who can reach the port.
- **A credential that is presented is checked in every mode, including on a
  read that required none.** Ignoring one wherever it was not needed is a
  registry that answers `200` to `Authorization: Bearer wrong`, which leaves a
  misconfigured client nothing to discover before its first push. Under `write`
  there is no read key, so a wrong credential there is a `401 UNAUTHORIZED`
  rather than the `403` that `all` gives a genuine-but-insufficient one.

  What it does not buy, because the assumption is natural and wrong: under
  `write` it does **not** make `docker login` or `oras login` validate a key.
  Both ping `GET /v2/` without a credential and send one only when answered
  `401`; here they are answered `200`, so nothing is sent and nothing is
  rejected — `login` succeeds on any key and the push is the first check.
  Verified against `oras` 1.3: login with a deliberately wrong key reports
  `Login Succeeded` under `--auth write` and `unauthorized: invalid API key`
  under `--auth all`.
- **The write key reads as well.** A CI job that pushes also pulls, and making
  it hold two secrets to do so is how one of them ends up unrotated. Write is
  also tried *first*, so setting both keys to the same value is one key that can
  do everything rather than one that has silently lost its write access.
- **The read key on a write is `403 DENIED`, and carries no challenge.** The
  credential is genuine, so re-challenging tells the client to retry with the
  same key it just used. `401` with the challenge is for a missing, unparsable
  or unrecognised credential — the three cases where trying again could work.
- **A supplied key is never printed; a generated one is printed exactly once.**
  The banner says which keys it had to invent, and stays silent about the rest —
  whoever passed a key already has it, and echoing it only copies a live
  credential into a scrollback, a log file and a CI transcript. `ApiKey`'s
  `Debug` redacts, so a `{:?}` on `ServerConfig` cannot leak one either, and
  comparison is constant-time so the timing cannot.

Two smaller things that are load-bearing anyway. A key supplied to a mode that
does not require it — either key under `--auth none`, `--read-apikey` under
`--auth write` — is a **startup error**, not a warning: the alternatives are to
ignore it, which serves a more open registry than its operator believes, or to
infer the mode from the key's presence, which makes deleting an environment
variable silently downgrade authentication. And the check runs as a layer
*before* any body is read, so a denied blob `PUT` is rejected without draining
the gigabytes behind it.

Deliberately not built: per-repository scopes, more than one identity, a token
endpoint, and key rotation without a restart. The keys are a shared secret in
front of a whole registry, which is the right size for the thing this is — a
binary someone runs to have a registry a minute later — and the wrong size for a
multi-tenant one. `summ-server/tests/auth.rs` is what stops a new route being
added outside the policy.

## Digest algorithms

- **The closing `?digest=` decides the algorithm, not the opening hint.**
  `?digest-algorithm=` (end-4c) is a SHOULD and no real client sends it — every
  blob flow in the conformance suite carries a `// TODO: add digest algorithm if
  not sha256` — so a sha512 push arrives on a session that has been hashing
  sha256. Rejecting it blames the client for content that is fine. The session
  rehashes its staged bytes instead (`Upload::rehash_as`), before the closing
  chunk, so the hasher simply carries on from the recorded offset.
- **The rehash is the one place staged bytes are re-read, and it must stay
  exceptional.** A session whose algorithm already matches returns without
  touching the file, which is every ordinary push. The rejected alternative was
  hashing both algorithms on the way in: correct, streaming, and it would make
  sha512 the bottleneck on *every* byte of *every* push — around 700 MB/s
  against sha256's ~2 GB/s with SHA-NI — to serve a case almost nobody takes.
  One sequential pass over a file written moments ago, only when the algorithms
  disagree, is the cheaper trade. This is emphatically not zot's re-read of
  every layer out of S3; do not let it grow into one.
- **A rehash must never rescue a wrong digest.** It changes which hash is
  computed, never what the commit compares against.

## Foreign layers

- **`urls` on a descriptor exempts it from reference validation, and from every
  edge.** A non-distributable layer names where its content actually lives and
  the registry is not expected to hold it. Demanding the blob rejects every
  Windows base image. Writing `L`, `P` or `R` for it would be worse: those keys
  are exactly what make a blob servable, so the edges would advertise bytes that
  are not on disk and turn a pull into a failed read rather than an honest 404.
  Absent and foreign means no validation and no edges; present anyway — a client
  may push one — and it is an ordinary blob from there on.
- **Foreignness is decided by `urls`, not by media type.** The
  `nondistributable` media types are the conventional carriers, but `urls` is
  what says where the bytes are, and keying off the media type would still
  reject a foreign layer wearing an ordinary one. An empty `urls` names nowhere,
  so it is not foreign.
- **A foreign layer is counted but not sized.** It stays in
  `ManifestRecord.layers`, because the manifest genuinely references it, and it
  is excluded from `total_layer_size`, because a repository's size means bytes
  this registry stores. Counting a base layer hosted on someone else's CDN would
  inflate every repository that mentions it.

## Work packages (parallelisable)

| # | Package | Owns | Depends on |
|---|---|---|---|
| A | **Conformance harness** — the suite passes by hand; this is putting it in CI so it is a gate | `conformance/` | — |
| F | Purge | `summ-purge/` | E |
| G | Scale benchmark — synthetic 10M-repo dataset, engine A/B | `benches/` | — |
| H | Extension API — *first cut done*; blob fan-in, untagged set, history remain | `summ-server/` | E |
| I | Built-in web UI — *first cut done*; assets embedded, same port | `summ-server/ui/` | H |

Done: **B** HTTP skeleton, **C** blob store, **D** upload sessions, **E** ops
layer, **J** pull counts, **K** wiring, and the first cut of **H** and **I**.
The Phase 1 exit criterion is met; package A is what stops it from silently
regressing.

**J did not get its own crate, and the table above no longer says it should.**
The aggregation is a `WriteBatch` builder, which is what `summ-registry`
already is, and the worker is a tokio task, which belongs beside `backend.rs` —
the one module allowed to name the lower crates. A crate for that would hold one
task and force `summ-server`'s layering rule to grow a fourth name. The
accumulator does sit in its own module (`summ-server/src/counters.rs`) above the
seam, because what is counted is an HTTP fact.

## Engine choice — RocksDB (decided)

**RocksDB for v1**, compiled by `librocksdb-sys` and statically linked, so the
registry ships as one binary. Verified: the only dynamic dependencies are
OS-provided. Full working in `research/R3`.

**Why an LSM.** Reads are point lookups and short prefix scans, and
prefix-*existence* checks are the purge hot path. Each push inserts tens of
**randomly distributed** keys (digest-prefixed, so effectively uniform) — where
a B-tree is weakest and an LSM absorbs into sequential writes. Bulk deletes
become cheap tombstones. Block compression matters more than usual, because most
of the keyspace is valueless edge keys sharing long digest prefixes.

**Sizing — measured.** On 20 M synthetic edge keys: 79 B/key raw → **53.12 B
with no compression at all**, purely from RocksDB's block key delta encoding →
**42.00 B** with zstd and 16 KiB blocks. Delta encoding does more work here than
compression does.

**The dominant unknown is not a knob — it is the blob fan-out distribution.**
References per blob at the same key count: 1 → 78.24 B/key, 2 → 55.55,
10 → 42.00, 100 → 39.41. So `R` is governed by how widely blobs are shared in
the real corpus: a planning range of **800 GB to 1.6 TB**, wider than every
tuning option combined. See Risk 2.

**Tuning — settled and applied** in `RocksEngine::open`. The headline:
**RocksDB's default `filter_policy` is `nullptr`** (`table.h:590`), so before
this the engine had no bloom filters at all.

- **Custom `SliceTransform` (`summ.prefix.v2`).** RocksDB's prefix-consistency
  property *is* satisfied by a variable-length transform, because the bytes
  deciding the length — the type byte, and for digest-bearing keys the algorithm
  byte — are themselves inside the prefix. There are **six** prefix lengths, not
  the four first assumed: 1, 5, 34, 38, 66, 70.
- **118 829 → 735 796 negative `exists_prefix`/s, a 6.2× win** on the purge hot
  path, and the prefix filter costs **10× less space** than a whole-key filter
  (0.125 vs 1.25 B/key), holding one entry per group.
- **Whole-key blooms do nothing for `exists_prefix`** — it is a seek, not a
  point lookup. Kept for `get`.
- **16 KiB blocks** shrink the index ~4× (11.2 GB → 2.9 GB projected); that, not
  compression, is the reason to raise it. **512 MiB LRU block cache** — the
  default is 32 MiB.
- **Rejected:** `optimize_for_point_lookup`, `optimize_filters_for_hits` (drops
  exactly the filters purge needs), universal compaction, zstd dictionaries
  (measured *worse*), and column families — cross-CF `WriteBatch` **is** atomic
  (12 rounds of SIGKILL during ~7000 three-CF batches, zero torn), but there is
  no merged cross-CF iterator and they leak a RocksDB concept into `MetaOp` and
  the replication log for nothing the transform does not already give.
  `auto_prefix_mode` is unusable: not exposed by the binding, documented `BUG:`
  in 11.8.1.

**Interning manifest digests to a `u32`**, as repo names already are, would
roughly halve the `R` range — a bigger win than every tuning knob combined. Not
adopted; it is a schema change with its own costs, and wants its own measurement.

**redb** is a second `MetaEngine` implementation, not a fallback. The whole
integration suite runs against both, which keeps the trait honest and the
decision reversible. If it becomes a drag, delete it — the tests are the asset.

## Blob storage — what to take from distribution

**Take the content-addressed blob store. Reject the link structure.**

distribution fuses two things in `registry/storage/paths.go`: a content-addressed
store, and *link files* whose paths encode repo→blob membership, tag→manifest
and revisions. Those exist because **distribution has no metadata database** —
it encodes relationships as paths so it can run on S3 alone. We have RocksDB,
where they live as `P`, `R`, `T` and `G`. Reproducing them on disk means two
sources of truth that silently diverge, and it is exactly what makes
distribution's GC a full storage-tree walk and its catalog a recursive directory
listing — LIST storms on S3. Slow catalog and list operations are the reason
this project exists.

So: a pure content-addressed store with **no directory structure that carries
meaning**, `digest → bytes`. That keeps the driver trait small, which is what
makes an S3 driver clean later. Four deliberate deviations:

0. **Unix-only.** `pread`/`pwrite` via `std::os::unix::fs::FileExt`, per R2.
   Linux is the deployment target, macOS the development one.
1. **Deeper fan-out, no per-blob directory.** distribution's
   `blobs/sha256/ab/<hex>/data` is 256 first-level buckets (~400K subdirectories
   each at 10⁸ blobs) and the per-blob directory doubles inode count to hold one
   file. Use `blobs/sha256/ab/cd/ef/<hex>`: 16.7M buckets, ~6 blobs each, and
   the file *is* the blob.
2. **Model `commit_upload`, not `move`.** distribution's `Move` is a lie on S3 —
   no rename, so it degrades to copy-then-delete, and copying a multi-gigabyte
   layer to commit it is pathological. The driver says "commit this upload as
   this digest": the filesystem renames, an S3 driver completes a multipart
   upload straight at the final key. **The most important trait-design lesson
   from distribution, and it is a lesson by counter-example.**
3. **Upload session state lives in RocksDB**, under `U` keys — cheaper to scan
   than distribution's `_uploads/<id>/startedat` files, and already
   transactional with everything else.
4. **Resumable hashing ports.** `crypto-common` 0.2's `hazmat::SerializableState`,
   implemented by `sha2` 0.11: **104 bytes** for sha256, 208 for sha512, and a
   rehydrated hasher gives an identical digest. Better than distribution's
   arrangement because it fits in the `UploadSession` record rather than needing
   files on the storage driver — so an interrupted chunked upload resumes on any
   process, removing what would be an HA constraint. Needs `sha2` 0.11+; the
   trait does not exist in 0.10. The state is sensitive and must not escape the
   metadata store.

### Crash consistency

One ordering rule, and it is not negotiable: **blob bytes land and are fsynced
first; the metadata batch is the commit point.** A blob with no metadata is
harmless garbage that purge reclaims. Metadata referencing a missing blob is
corruption that surfaces as a failed pull. Never the reverse.

**Creating a fan-out directory races, and a level another writer created first
is a success.** Every check in `create_dir_durable` is a check-then-create, and
any two blobs share at least `blobs/<algo>`, so two commits into a fresh store
both find it missing and both create it. That is not a rare interleaving: `oras
push` uploads an artifact's blobs in parallel, so it is what an ordinary push
does, and taking `EEXIST` out to the caller made it a `500` with the layer
already uploaded. `AlreadyExists` is absorbed — **and the fsync still runs on
that path**, because the winner has not synchronised with us and "it is
presumably about to sync the same parent" is not a durability argument. A
non-directory holding the name is *not* absorbed: a level is two hex characters
and a blob file is the full hex, so it can only mean a corrupted store, and it
must surface here rather than as a baffling `ENOTDIR` from the rename.

## Replication and the WAL

`WriteBatch` is already the log: a serialisable, self-contained description of a
change. A WAL means persisting each batch alongside applying it, with no change
to callers. What makes that work is that batches contain only `Put`, `Delete`
and `DeletePrefix` — no read-modify-write anywhere, because the schema has no
fan-in vectors needing one. Had we kept the prototype's `merge`, replay would
have required exactly-once delivery.

**One correction to an earlier, too-strong claim.** `Put` and `Delete` are
self-contained; `DeletePrefix` is not — its blast radius depends on what is in
the store when applied. So the log is safe to replay **in order, from a
consistent point**, and an overlapping suffix replayed in order converges. It is
*not* safe under arbitrary reordering, nor against a store a second writer is
mutating outside the log. Do not call these batches simply "idempotent" without
that qualifier; the WAL design must guarantee ordered replay.

The two constraints that protect this — no side-channel writes, no
non-deterministic content in a batch — are in CLAUDE.md.

Blobs need no WAL: content-addressed and immutable, so replication is plain
copy, or shared object storage.

## Pull counts

Served at `GET /api/v1/pull-counts/<name>` and
`.../<name>@<reference>`, with two contribution grids on the repository, tag and
manifest pages: thirty days, and the last twenty-four hours. On by default;
`--no-pull-counts` (`SUMM_NO_PULL_COUNTS`) turns off the recording, not the
endpoint. The feasibility working is in **`design/analytics.md`**; what follows
is what was actually built and what a change must not undo.

**Nothing in `MetaEngine` changed.** No merge operator, no reverse scan, no new
`MetaOp`. That was the test the design had to pass and it passed it.

- **The pull path never touches the store.** A served `GET` locks a
  `HashMap<(subject, day, hour), counts>` in `summ-server/src/counters.rs`, adds,
  and returns. A background task drains it every five seconds and folds it into
  the `A` range in one `WriteBatch` of plain `Put`s. **The map holds deltas since
  the last flush, not running totals** — the design note proposed totals seeded
  from the store and held forever, which is unbounded in the ten-million-repo
  direction and needs an eviction policy to fix. Deltas bound the map by the
  traffic in one interval instead, there is nothing to seed, and a restart costs
  one interval rather than leaving stale totals behind.
- **There is no queue and no channel**, which the design note also proposed. A
  channel costs an allocation and a task wakeup per pull and drops events under
  exactly the burst you most want counted; a map turns a burst on one manifest
  into repeated increments of one entry. What is bounded instead is the number
  of *distinct* buckets (`MAX_BUCKETS`, 50 000), so saturating costs the long
  tail of a spike and never the volume of a hot subject.
- **This is the one read-modify-write in summ, and it is where the rule is paid
  for rather than a hole in it.** The fold happens in the flush, *before* the
  batch is built, so what reaches the log is an absolute value with no
  engine-minted content — replayable, exactly as the rule requires. What the
  rule forbids is a read-modify-write on the request path, and there is none.
- **Best-effort, and every response says so.** A crash loses up to one interval;
  a saturated accumulator drops new subjects. `approximate: true` is a constant
  field on the endpoint precisely so nothing has to remember to mention it.
  There is deliberately no drain on shutdown — what would be lost is under one
  interval of a signal already declared approximate, and the alternative is a
  shutdown path that can block on the metadata store.
- **A flush resolves the repository name and skips what it cannot find.** It
  calls `lookup_repo`, never `intern_repo`: a pull can only have been served for
  a repository that existed, so a miss means it was deleted in between, and
  minting an id would resurrect a name in the catalog on the strength of a
  counter. Counters left behind by a later delete are harmless — `A` is a clean
  prefix under the repo and purge drops it with a `DeletePrefix`.

**What counts, decided once so the numbers mean something.**

- `GET /v2/<name>/manifests/<ref>` is the pull event, counted against the
  manifest, the tag (when the client asked by tag) and the repository. **`HEAD`
  is not**: containerd issues `HEAD` then `GET` on every cold pull, so counting
  both doubles every number. A `304` is not either.
- A multi-platform pull is two `GET`s, the index and the chosen child, and each
  is counted against itself — so the index's grid is "how often was this image
  pulled" and the children's are the platform split. That falls out of the rule
  rather than needing a case.
- Blob `GET`s add `blob_pulls` and `bytes_out` **on the repository scope only**.
  Attributing a shared layer's bytes to one manifest would be a lie, and doing
  it honestly needs `R`, which is a scan.
- **`bytes_out` is metered, not promised.** The response body is wrapped and
  reports on drop, so what is counted is what reached the socket. containerd
  2.1+ asks for `bytes=N-`, reads 8 MiB and tears the connection down; counting
  the requested window would over-report a 900 MB layer about a hundred times.

**The value is per hour, and there is no stored day total.** `CounterBucket`
holds three fixed 24-element arrays; the day figure is their sum. That is 24
additions and it cannot disagree with the parts, where a cached total beside
them would be a second source of truth for one number. The hours are what make
the numbers honest outside UTC: **a day bucket is fixed at write time and must
never be re-bucketed**, or the same wall changes shape depending on who is
looking at it, but an hour names an *instant* and can be re-summed into any
zone. The UI does exactly that — the daily grid is labelled UTC and stays UTC,
the 24-hour strip is relabelled to the viewer's clock. The weekday is
`(day + 4) % 7`, arithmetic on the key, because 1970-01-01 was a Thursday;
nothing is stored for it.

Adding the arrays was free because nothing had ever written an `A` key. After
the first one it would have been a `DBVersion` migration on records postcard
cannot decode — the same trap `skip_serializing_if` sprang.

**The endpoint is a window, not a cursor.** `?days=` (default 30, clamped to
400) sets the length and it always ends today, so the whole visualisation is one
ordered scan of at most a few hundred keys. Every day in the window comes back
whether or not it saw traffic, because the client is a grid and a gap in a grid
is a missing cell rather than a zero one. Nothing 404s, for the reason tag
history does not: counts outlive what they describe.

**Not built, and not a package: no per-pull event log.** It is the only
structure here that would grow with traffic rather than with content, and it
buys only retroactive re-bucketing — which the hourly arrays now mostly give
anyway, for a number already declared approximate.

**Open: retention**, along with `H` and `J`. See **Tag history**.

**Open: the aggregation worker is one process-local task**, so the reserved
`<shard>` key component is `0` and the read path sums across shards. Two nodes
each flushing absolute values into one bucket is last-write-wins and therefore
silent undercounting; the component is there so that adding a second writer is
a configuration change rather than a migration.

## Risks

0. **A lost or corrupt RocksDB is a dead registry** — **partly mitigated**. Tags
   live only under `T`, so the metadata store is still authoritative with no way
   to reconstruct it. zot does not have this problem — its metadata is a derived
   cache rebuilt by walking storage — which is also why its migration patch lists
   are empty: it can always rebuild. Both mitigations R4 proposed have now
   landed: the schema version marker plus migration hook (`summ-meta::version`),
   and **the manifest copy**.

   **The corpus is self-describing.** A manifest push writes its document into
   the blob store under its own digest as well as under `B`, fsynced before the
   batch like any other blob (`Backend::archive_manifest`). A manifest is
   content-addressed already, so this is `digest -> bytes` and invents no
   concept: recovery walks the blob store, finds the documents that parse as
   manifests, and from each one gets its media type, its config and its layers.

   What it does **not** recover: tags, repository membership, `pushed_at`, and
   which repository anything belonged to. All of that is `T`, `P` and `M`, and
   the planned WAL is what covers it. Nor is a recovery sweep written — this is
   the raw material for one, not the tool.

   **The consequence for purge (package F), which must not be discovered by
   accident.** The copy carries no `L` and no `P` record, deliberately: those
   make bytes servable through `GET /v2/<name>/blobs/<digest>` and fold them
   into the repo's blob count and byte total, and a manifest is not a blob of
   its repository. So the copy looks exactly like garbage to a sweep that asks
   only "does anything reference these bytes". **Purge must retain a blob whose
   digest appears as a manifest digest.** `M` is the record that keeps it; an
   offline sweep walks `M` to build the live set anyway, so this costs it
   nothing. No second record was invented for it, because the blob store holds
   bytes and not relationships. For the same reason a manifest delete leaves the
   copy alone — `M` is repo-scoped and the store is global, so only the sweep
   can tell that nothing names it any more.

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
4. **`A`, `H` and `J` are the only ranges that grow with time rather than with
   content**, and absolute-value counters are last-write-wins under two writers.
   Neither bites on a single node. The shard component is reserved and written
   as `0`, with the read path summing across shards, so a second writer is a
   configuration change; the retention window is still unwritten and belongs to
   the purge sweep. See **Pull counts** and **Tag history**.
5. **Offline purge is a scaling cliff.** Acceptable for v1 by decision. The
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

## Open questions and deferred work

The `research/R4` schema batch is applied; the record of what changed and why is
in **`design/applied-schema-changes.md`**. What is still open from it, and what
implementation has since turned up, is below.

**Deferred, deliberately:**

- **Manifest-digest interning to a `u32`** — measured by R3 as roughly halving
  the `R` keyspace, a bigger space win than every tuning knob combined. Still
  not adopted: it is a schema change with its own costs and it wants its own
  measurement first.
- **S3 multipart identifiers on `UploadSession`** — a value change, not a key
  change, and there is no S3 driver until Phase 5. The version marker that
  landed in this batch is exactly what makes adding them later safe.

**Found while wiring (package K), still open:**

- **Metadata reads run inline on the reactor; only writes get `spawn_blocking`.**
  `summ-registry` is synchronous throughout, and a write reaches RocksDB's WAL,
  so `Backend::write` moves those off the reactor. Reads are left inline on the
  bet that they are block-cache hits measured in microseconds and that a
  `spawn_blocking` round trip (~5 µs, measured in R2) would cost more than the
  lookup it protects. That bet is unmeasured, and a cold read on a
  ten-million-repo store is exactly where it would be wrong. Phase 3.
- **Anonymous cross-repo mount is served from `L` alone.** `?mount=<digest>`
  with no `from=` grants membership on the strength of "the content exists
  somewhere", which is what the spec's anonymous mount means and what makes it
  one lookup. It is also the only place `L` decides anything a client can
  observe, so if a private-repository model ever arrives, this is the line that
  has to change.
- **A push rejected mid-body leaves staged bytes behind.** The declared-length
  check moved into the body consumer when pushes stopped being buffered, so a
  short body is detected after some bytes have reached the staging file. The
  session record is not written, so the recorded offset is unchanged and the
  next resume truncates the excess — the client sees exactly what it saw before.
  Worth knowing when reading the staging directory, and worth a purge sweep for
  the case where the client never comes back.

**Found while adding the manifest copy (Risk 0):**

- **The copy costs one uncompressed manifest per manifest, and nobody has
  sized it.** `B` holds the document zstd-compressed; the blob store holds it
  as pushed, because it is content-addressed and the digest is over the
  plaintext. At Risk 2's assumed ~10⁹ manifests and a couple of kilobytes each
  that is the same order as the `R` keyspace, which makes it a real number and
  not a rounding error. Package G should measure it alongside the rest.
- **A repository's reported `size_bytes` no longer matches its footprint.**
  The count folds `P`, and the copy deliberately has no `P` record, so the
  manifests a repository owns are on the disk and not in the number. Correct as
  a definition — those bytes are not blobs of the repository — and worth saying
  out loud before somebody reconciles the two and "fixes" it.
- **Two writes of the same manifest race harmlessly, and only because the
  content is addressed.** Concurrent pushes of one document both see
  `contains` false, both stage, and both rename onto the same path; the second
  rename wins and the bytes are identical either way. Nothing here relies on
  that beyond what `commit_upload` already documents, but it is the reason no
  lock appears on this path.

- **The manifest push now plans and applies in two steps, with disk I/O in
  between.** `Backend::put_manifest` plans its batch, writes the archive copy,
  then applies — so validation reads (reference checks, and the `exists_prefix`
  that decides whether `P`'s `added_at` grace clock is rewritten) are separated
  from the apply by an `await` on a create-append-fsync-rename, which is
  milliseconds rather than the microseconds the single closure used to be. The
  race is not new — `write` is a `spawn_blocking` hop with no lock, so two
  pushes have always been able to interleave against the engine — but the window
  is now wide enough to hit. The bad outcome is a manifest committing against a
  blob that was deleted after it was validated, which is metadata pointing at
  missing content. Options: re-check references inside the applying closure, or
  make the push path take the repo write lock the WAL will want anyway. Note the
  ordering rule forces the copy to precede the batch, so the fix is not
  "reorder"; and per the rule in **What the client actually does**, any lock
  here must never be held across a request-body read.

**Found while building the discovery API and UI (packages H and I):**

- **An image manifest's platform is not in the store, and the UI shows it as
  blank.** `ManifestRecord.platform` is only ever populated for an index's
  children: an image manifest carries no platform of its own — it is
  `architecture`/`os` in the *config blob* — and reading it would put a blob
  fetch on the push path. So a repository of standalone single-arch images shows
  no platforms at all. Options, none taken yet: store the config's platform at
  push time (one extra blob read per push, and only for manifests whose config
  is already present), or accept it. Related to the `config_media_type` item
  below — both want the same read.
- **`COUNT_CEILING` is 10,000 and unmeasured.** It bounds a fold, so the worst
  case for a repository-list page is `page_size × 2 × CEILING` key reads
  (100 × 2 × 10,000 with a maximal `?n=`). That is bounded, which is the point,
  but nobody has measured what it costs on a store at target scale. Package G.
- **The discovery API has no `HEAD`-cheap variant of a count.** A UI that wants
  only "how many repositories" still pages the list. Not a problem at the sizes
  reached so far; worth a `/api/v1/registry` summary if it becomes one.

**Found during earlier implementation, still open:**

- **`ManifestRecord.artifact_type` cannot answer a referrers query on its own.**
  The value the response must carry is the *effective* artifact type: an image
  manifest with no `artifactType` reports its **config descriptor's
  mediaType**, which the record does not store. Free on the push path, where the
  parsed body is in hand — but synthesising an edge for a cosign tag set after
  the fact forces a re-read and re-parse of `B`. A `config_media_type` field, or
  redefining `artifact_type` as the effective value, removes that read.
- **`summ_core::keys` has encoders but no decoders** for the digest-bearing
  suffixes (`M`, `P`, `F`, `S`, `R`). Every paged query needs them to turn an
  engine cursor into a URL-safe token. They currently live in
  `summ-registry/src/suffix.rs` and belong next to the encoders.
- **`SummError` has no `Io` variant and no `OffsetMismatch { expected, got }`.**
  An out-of-order upload append — which the HTTP layer must turn into a 416 —
  currently rides on `InvalidData`. Matchable without parsing messages, but a
  dedicated variant is the honest end state. The wiring works around it rather
  than fixing it: `Backend::append_upload` compares the session's own offset
  before touching the file, so the storage error never has to be classified.
- **`DigestAlgorithm` lives in `summ-storage`, not `summ-core`.** An upload
  picks an algorithm at POST time from `?digest-algorithm=`, before any hash
  exists, and `Digest` can only name one after the fact. It belongs beside
  `Digest`.

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

**R1–R6 are all done** and their findings are folded into the sections above.
Read a file when you need the working behind a decision, not before.

| File | Question |
|---|---|
| `research/R1-spec-conformance.md` | What must summ implement to pass conformance? |
| `research/R2-zero-copy-serving.md` | How should blob bytes reach the socket? *(Do not zero-copy.)* |
| `research/R3-rocksdb-tuning.md` | Prefix bloom filters given six prefix lengths |
| `research/R4-zot-prior-art.md` | What has zot already solved? |
| `research/R5-clients-and-ecosystem.md` | What does the real client do? Plus the Rust ecosystem survey (R6). |

Open, not research: **aggregate manifest count** (Risk 2) — the per-axis bounds
do not size the store.

## Reference material (sibling directories)

| Path | Use |
|---|---|
| `../distribution-spec` | Spec text + `conformance/` suite — the acceptance gate |
| `../distribution` | Reference impl. `registry/storage/paths.go` for the on-disk layout |
| `../zot` | Closest prior art: `pkg/meta/boltdb` (embedded metadata store), `pkg/storage/{types,s3,local}` |
| `../harbor` | Product-level features, not architecture |
| `../summ-bench` | **Your** bench harness. `notes/fs_limit.md` has the capacity analysis |
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
schema. **Built** means served on `/api/v1/` and used by the UI; the ops-layer
query exists for every row here either way.

| Query | Backed by | Status |
|---|---|---|
| List repositories | `n` (name-ordered) | **built** |
| Search repositories by name prefix | `n <prefix>` | **built** |
| List tags in a repo | `T <repo>` | **built** |
| List manifests in a repo, with counts | `M <repo>` | **built** |
| Repo size and manifest count | `P <repo>` | **built** |
| Which tags point at this manifest | `G <repo> <digest>` | **built** |
| Which manifests reference this layer | `R <digest>` | ops layer only |
| Untagged / reclaimable manifests | `M` minus `G` | ops layer only |
| Pull counts by day and hour, for a wall | `A m <repo> <digest>` | **built** |
| Tag history, newest first | `H <repo> <tag>` | **built** |
| What was this manifest ever tagged | `J <repo> <digest>` | **built** |

summdb prototyped several of these (`/v1/repos/:repo/stats`,
`/v1/layers/:digest/manifests`) along with the UI that consumes them; that code
is the starting point.
