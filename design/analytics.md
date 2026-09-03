# Analytics — pull counts and tag history (design sketch)

Lifted out of PLAN.md, which is loaded into every session and should not carry
the full working for an unbuilt feature. Nothing here is decided. It is the
feasibility argument for one question — **can the key schema hold pull counts
and tag history without a redesign?** — and the answer is yes, which is why the
`A`, `H` and `J` ranges already have key builders, value types and prefix
groups with nothing writing to them.

Read this before scheduling package J. The endpoints, the retention policy and
the aggregation details are all still open.

## Analytics — pull counts and tag history

Both are wanted, both are later-phase features, and neither is designed here.
What this section settles is the narrower question: **can the key schema hold
them at all, without a redesign when the time comes?** It can, and the working
below is a feasibility sketch rather than a specification — key shapes, the
constraints they have to respect, and the places where a choice would be
expensive to reverse. The endpoints, the retention policy, and the aggregation
details are all open, and should be designed properly when the feature is
actually scheduled.

### The invariant this has to survive

A counter is a read-modify-write, and there is no read-modify-write anywhere in
this design: no `merge`, no side-channel writes, and every batch must mean the
same thing wherever it is replayed. The resolution is to **aggregate in memory
and persist absolute values**:

- The pull path pushes `(repo, digest, kind)` onto a bounded in-process queue and
  returns. It never touches the store, never blocks, and **drops events when the
  queue is full** — a pull must not slow down, and must never fail, for a counter.
- One background worker owns the aggregate. It holds `(scope, day) -> counts` in a
  map, seeding an entry from the store with a single `get` the first time it
  touches that entry after start-up.
- Each flush interval it emits **one `WriteBatch` of plain `Put`s carrying the
  current absolute value**. No merge, no delta, deterministic content, safe to
  replay, and O(dirty buckets) rather than O(pulls).

So the analytics feature needs no new engine primitive at all. What it needs is a
writer that is allowed to hold state in RAM, which the `MetaEngine` contract has
never forbidden.

Two consequences to accept out loud:

- **Pull counts are best-effort.** A crash loses up to one flush interval; a full
  queue loses the peak of a spike. They are a popularity signal for an operator,
  not billing data. The API should say so rather than implying exactness.
- **Tag history is not best-effort**, and does not go through this pipeline at
  all. See below.

### Tag history — transactional, and shaped by the spec that may arrive

`opencontainers/distribution-spec#606` proposes
`GET /v2/<name>/_oci/tag-history/<reference>`: descending chronological order,
`n` / `before` / `since` query parameters, and a response that is an OCI image
index whose entries carry `org.opencontainers.distribution.tag.timestamp` and
`.tag.event` (`created` | `deleted`) annotations. It is unmerged and the data
model is still argued over, but the *storage* requirement it implies is stable,
and one key range serves it:

```
H <repo> <tag> 0x00 <!timestamp:8> <digest>  ->  TagEvent { event, media_type, size }
```

Five things are doing work in that key:

- **`0x00` is the separator.** Tag names match `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`
  (`distribution-spec/spec.md:160`), so NUL cannot occur in one, and it sorts
  below every legal tag byte. Without it a scan of tag `foo` would also sweep up
  `foobar`'s history. `T <repo> <tag>` needs no such separator only because the
  tag is terminal there.
- **The timestamp is stored complemented** (`!ts`), so newest sorts first.
  The spec wants descending order and `MetaEngine` has only a forward `scan` —
  there is no reverse iterator in the trait, and adding one to both engines to
  serve one endpoint is not worth it. Complementing costs nothing.
- **`before` and `since` are then just `start_after`.** The spec's cursor model
  falls straight out of the key encoding: `before=T` is a seek to
  `H <repo> <tag> 0x00 !T`. There is no cursor token to invent and nothing to
  keep consistent between pages.
- **The digest is in the key, not only in the value**, because it is the
  collision breaker. Simultaneous events sharing a timestamp is the open
  objection on the PR; two events on one tag at the same instant with the same
  digest are the same event, so putting the digest in the key closes it without a
  sequence counter.
- **The descriptor is denormalised into the value**, exactly as `F` /
  `ReferrerRecord` already is. Not for speed: the spec requires history to remain
  queryable after the manifest is deleted, so `M <repo> <digest>` may no longer
  exist to look it up in. This is bounded fan-out — one event's own descriptor —
  so it does not violate the no-growing-values rule.

These events are written **in the same `WriteBatch` as the tag mutation itself**,
never through the analytics queue. A dropped history record is a hole in an audit
trail; a dropped pull count is a rounding error. Retagging writes one `created`
event, per the PR; an explicit tag delete writes one `deleted` event carrying the
digest that was displaced, which `T <repo> <tag>` supplies at that moment.

`<reference>` in that endpoint is a tag **or a digest**, and the digest form is
also the question the UI wants to ask ("what was this manifest ever tagged, and
when"). It needs the other direction:

```
J <repo> <digest> <!timestamp:8> <tag>  ->  TagEvent
```

One extra key per tag event. Whether to write both directions from the start, or
only `H` and accept that the digest form arrives later without back-history, is a
question for the feature design — worth noting only because it is the kind of
choice that is cheap up front and awkward afterwards.

### Pull counts — day buckets, one key per bucket

```
A <scope> <...> <day:2> <shard:2>  ->  CounterBucket { manifest_pulls, blob_pulls, bytes_out }
```

Three scopes, all written by the same worker in the same flush:

| Key | Answers |
|---|---|
| `A m <repo> <digest> <day> <shard>` | the per-manifest wall — the headline feature |
| `A t <repo> <tag> 0x00 <day> <shard>` | which tags people actually pull |
| `A r <repo> <day> <shard>` | repo totals |

- **`day` is a big-endian `u16` of days since the Unix epoch, in UTC** (good to
  2149). The bucket boundary is fixed at write time. The UI may relabel to a local
  zone, but it must not re-bucket, or the same wall changes shape depending on who
  is looking at it.
- **The contribution-wall query is a single bounded scan.** 53 weeks is 371
  buckets, so the entire visualisation is
  `scan(A m <repo> <digest>, start_after = <cutoff day - 1>, limit = 371)`,
  arriving in chronological order, with the handler zero-filling the gaps. No
  pagination, no read-time aggregation, no unbounded set — the one query the
  feature exists for is also the cheapest thing in the store.
- **Every granularity you intend to query must be maintained on write.** Rolling
  repo totals up out of per-manifest buckets would be a scan across up to 10M
  manifests: an unbounded scan wearing a summary's clothing. Three scopes cost
  three extra `Put`s per flush; discovering the rollup is impossible costs a
  rewrite.
- **The value is a struct, not a bare `u64`.** Bytes served and blob pulls are
  wanted next to the pull count, and a second metric must not mean a second key
  range. Note this makes the analytics values the first records likely to *gain*
  fields later, which is the concrete argument for landing R4's `DBVersion` key
  and migration hook before this rather than after: postcard is not
  self-describing and will not decode a record written before a field was added.
- **`<shard>` is reserved for the writing node's id**, `0` on a single node. Two
  nodes each writing an absolute value for the same bucket is last-write-wins,
  which is silent undercounting rather than a visible failure. The topology
  decision is "single node, but keep HA viable"; reserving two bytes now is free,
  and adding a key component to a populated store is a migration.
- **No raw per-pull event log.** It is the only structure here that would grow
  with traffic rather than with content, and it buys only the ability to re-bucket
  retroactively — not worth it for a number already declared approximate.

**What counts as a pull**, decided once so the numbers mean something:

- `GET /v2/<name>/manifests/<ref>` is the pull event. `HEAD` is not — containerd
  issues `HEAD` then `GET` on every cold pull (R5), so counting both doubles every
  number.
- Pulling a multi-platform image issues two manifest `GET`s, the index and the
  chosen child. Count each against itself: the index's wall is "how often was this
  image pulled", and the children's walls are the platform split. That falls out
  of the rule rather than needing a special case.
- Blob `GET`s increment `blob_pulls` and `bytes_out` on the repo scope only.
  Attributing a shared layer's bytes to one manifest would be a lie, and the
  fan-in needed to do it honestly is `R`, which is a scan.

Rejected alternative: one key per manifest-year holding a 365-entry array. Fewer
keys, but it rewrites ~730 bytes on every flush, keeps every version alive until
compaction, and makes retention all-or-nothing. Per-day keys delta-encode almost
perfectly — consecutive days for one manifest differ only in the last bytes —
which is the case R3 measured at 42 B/key.

### What this costs the engine

- **New prefix groups in `summ_prefix_len`, and the extractor bumps to
  `summ.prefix.v2`.** `A <scope> <repo> <digest>` groups at 39 or 71 bytes,
  `A <scope> <repo>` at 6, and `H` / `J` at `<prefix> <repo>` = 5, alongside
  `M`/`B`/`T`/`P`. Prefix consistency still holds by the existing argument: every
  byte that decides the length — the type byte, the new scope byte, the digest
  algorithm byte — is itself inside the prefix. Land these together with the
  pending R4 changes so the extractor name moves once rather than three times.
- **Retention becomes something that exists.** Every other key range is bounded by
  current state; `A`, `H` and `J` are bounded only by time. Proposal: 400 days of
  `A m` and `A t` (a year of wall plus slack), `A r` kept indefinitely because it
  is tiny, and `H` / `J` trimmed only when the repo is deleted, which is what #606
  permits. Enforcement belongs to the purge sweep, which already walks the store.
- **Purge gains prefix deletes, no new machinery.** Dropping a manifest drops
  `A m <repo> <digest>`; dropping a repo drops all of `A`, `H` and `J` under that
  repo. All clean prefixes, so `DeletePrefix` covers them.
- **Nothing changes in `MetaEngine`.** No merge operator, no reverse scan, no new
  op in `MetaOp`. That is the test this design had to pass.
