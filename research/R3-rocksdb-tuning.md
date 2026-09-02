# R3 — RocksDB tuning for a four-prefix-length key schema

**Status: in progress.** Written incrementally; sections fill in as each finding
is established by experiment or by reading source.

Question: *how do we tune RocksDB for this key schema, given prefixes of four
different lengths?* The lever we most want is a **prefix bloom filter** serving
`exists_prefix()` — "is this blob still referenced?" — which purge asks once per
candidate blob, potentially billions of times.

## Outline

1. The actual prefix lengths (from `keys.rs`, not from PLAN.md's summary)
2. Option A — custom `SliceTransform` — **viable, recommended**
3. Option B — column families per key type (decisive question: cross-CF batch atomicity)
4. Whole-key bloom filters and what they do for a *seek*
5. Other levers — compression, block size, `optimize_for_point_lookup`, block cache, compaction style, `DeleteRange`
6. Space — what actually reclaims the `R` keyspace
7. RECOMMENDATION

---

## 1. The actual prefix lengths

PLAN.md says "1, 5, 34, 66". That undercounts. Enumerating every scan prefix
`keys.rs` actually constructs, with `RepoId = u32` (4 bytes) and
`Digest::encoded_len()` = 1 algorithm byte + raw hash = **33** (sha256) or
**65** (sha512):

| Scan prefix builder | Shape | sha256 | sha512 |
|---|---|---|---|
| `uploads()` | `U` | 1 | — |
| `repos_by_name()` | `n` | 1 | — |
| (`i`, `L` have no scan; point lookups only) | | | |
| `manifests_in_repo` | `M <repo:4>` | 5 | — |
| `tags_in_repo` | `T <repo:4>` | 5 | — |
| `blobs_in_repo` | `P <repo:4>` | 5 | — |
| **`blob_refs`** | **`R <digest>`** | **34** | **66** |
| `blob_refs_in_repo` | `R <digest> <repo:4>` | 38 | 70 |
| `tags_of_manifest` | `G <repo:4> <digest>` | 38 | 70 |
| `parents_of` | `S <repo:4> <digest>` | 38 | 70 |
| `referrers_of` | `F <repo:4> <digest>` | 38 | 70 |

So **six distinct lengths (1, 5, 34, 38, 66, 70)**, not four. Two facts fall out
that shape the whole answer:

1. **Length is not a function of the type byte alone.** It also depends on the
   digest algorithm byte — at offset 1 for `R`, at offset 5 for `G`/`S`/`F`.
   Any transform must read two bytes, not one.
2. **`R` alone needs two lengths.** `blob_refs` (34) is the purge hot path;
   `blob_refs_in_repo` (38) gates serving. A single extractor cannot be both.
   §2.3 shows why picking 34 is nevertheless the right answer and costs nothing.

Full stored `R` key is `R <digest:33> <repo:4> <manifest:33>` = **71 bytes**,
value empty. That is the key the ~10¹⁰-key / ~1 TB estimate is about.


---

## 2. Option A — a custom `SliceTransform`

### 2.1 Is it expressible through the `rocksdb` 0.25 binding? Yes.

`rocksdb-0.25.0/src/slice_transform.rs` exposes:

```rust
pub type TransformFn<'a> = fn(&'a [u8]) -> &'a [u8];
pub type InDomainFn      = fn(&[u8]) -> bool;

impl SliceTransform {
    pub fn create(
        name: impl CStrLike,
        transform_fn: TransformFn,
        in_domain_fn: Option<InDomainFn>,
    ) -> SliceTransform;
    pub fn create_fixed_prefix(len: size_t) -> SliceTransform;
    pub fn create_noop() -> SliceTransform;
}
```

Four things to notice in that source, all of which matter:

- **`in_domain` is fully supported.** `SliceTransform::create` always registers
  `in_domain_callback` with the C API; passing `None` just makes it return
  `true` unconditionally. Passing `Some(f)` gives us real domain control.
- **They are `fn` pointers, not closures.** No captured state, so the transform
  must be a plain `fn` item. That is fine here — the key schema is static — but
  it means the transform cannot, say, be parameterised by config.
- **`transform_callback` returns `prefix.as_ptr()` straight to C**, so the
  returned slice **must be a subslice of the input `key`**. Returning a
  `&'static` or freshly allocated slice would hand RocksDB a pointer with the
  wrong lifetime. Prefix extraction is naturally a subslice, so this is a
  constraint we satisfy for free — but it rules out any "normalising" transform.
- **There is no `Drop`.** The `SliceTransform` boxes its callback and leaks it
  (deliberately: "only used by people passing it as a prefix extractor when
  opening a DB"). One per process is fine; do not create these in a loop.

`Options::set_prefix_extractor(SliceTransform)` (db_options.rs:1922) takes it by
value. So yes — expressible.

### 2.2 Prefix consistency: what RocksDB actually demands, and whether we satisfy it

The authoritative statement is in `options.h` on
`ColumnFamilyOptions::prefix_extractor` (librocksdb-sys 0.19.0+**11.8.1**,
`rocksdb/include/rocksdb/options.h:257-289`). Quoted verbatim:

> Together `prefix_extractor` and `comparator` must satisfy one essential
> property for valid prefix filtering of range queries:
>   If `Compare(k1, k2) <= 0` and `Compare(k2, k3) <= 0` and
>      `InDomain(k1)` and `InDomain(k3)` and `prefix(k1) == prefix(k3)`,
>   Then `InDomain(k2)` and `prefix(k2) == prefix(k1)`
>
> In other words, all keys with the same prefix must be in a contiguous group by
> comparator order, and cannot be interrupted by keys with no prefix ("out of
> domain").

plus four recommended properties: *prefix is a prefix*, *prefixes preserve
ordering*, *prefix starts the group* (`Compare(prefix(key), key) <= 0`), and
*prefix idempotent* (`prefix(prefix(key)) == prefix(key)`).

**A type-byte-dispatched variable-length transform satisfies all five.** The
proof turns entirely on one fact: *the byte the length depends on is inside the
prefix*.

- Take `k1 <= k2 <= k3` with `prefix(k1) == prefix(k3) == P`, both in domain.
  `P` is at least 1 byte and always begins at offset 0, so `k1` and `k3` both
  start with `P`. Under bytewise ordering, any `k2` between two strings sharing
  prefix `P` must itself start with `P` (the only alternative, a string that
  diverges from `P` at some byte, sorts outside the interval). Therefore `k2`
  starts with `P`.
- `P` contains the type byte, and — for `R`, `G`, `S`, `F` — the algorithm byte
  too. So `k2` has the same type and the same algorithm as `k1`, hence the same
  target length `n = |P|`, and `|k2| >= |P| = n`, so `InDomain(k2)` and
  `prefix(k2) = k2[..n] = P`. ∎
- The domain decision is likewise a pure function of the first one or two bytes,
  so out-of-domain key types (`L`, `U`, `n`, `i`) occupy their own contiguous
  byte ranges and can never sit *between* two in-domain keys of the same prefix.
  This is the clause that would kill a "prefix = up to the first separator byte"
  style transform; it does not bite here.
- *Prefix is a prefix*: by construction. *Ordering preserved*: type byte first,
  so cross-type order is preserved, and within a type it is fixed-length
  truncation. *Prefix starts the group*: a prefix always sorts `<=` its string.
  *Idempotent*: `prefix(P)` re-reads the same type and algorithm bytes out of
  `P` and gets the same `n = |P|`, so `prefix(P) = P`.

For contrast, this is exactly the property `NewFixedPrefixTransform` relies on
and the reason RocksDB's own docs warn about comma-terminated prefixes: those
break *idempotence* and *contiguity*. Ours breaks neither.

**Verdict: prefix consistency does not kill Option A.** It is verified by
experiment in §2.4.

### 2.3 The `R`-needs-two-lengths problem is not a problem

`R` has two scan prefixes: 34 (`blob_refs`, purge) and 38
(`blob_refs_in_repo`, serving gate). Choose **34**.

When the seek key is *longer* than the extractor's prefix, RocksDB extracts the
34-byte prefix from the 38-byte seek key and filters on that. Filtering is
coarser than it could be, but a 34-byte prefix is `R` + a whole sha256 digest —
already maximally selective for the thing we care about. All of a blob's edges,
across every repo, live in one prefix group; the extra 4 repo bytes only narrow
*within* a group that is typically a handful of keys.

Choosing 38 instead would be strictly worse: the 34-byte purge seek would then
be *shorter* than the extractor prefix, `InDomain` would be false, and RocksDB
falls back to a total-order seek — correct, but with no bloom filter at all on
the one path we most want filtered.

Same reasoning for sha512: extractor 66, serving-gate seek 70.

### 2.4 Verified by experiment

Scratch crate: `rocksdb = "0.25"`, release build, the transform below verbatim.
Full source in the appendix (§8). Result:

```
[memtable]
  ok   exists R<blob1> (34B) = true
  ok   exists R<blob3> (34B, absent) = false
  ok   exists R<blob1><repo5> (38B) = true
  ok   exists R<blob1><repo7> (38B, absent but blob IS referenced) = false
  ok   exists R<blob2><repo9> (38B, absent) = false
  ok   exists R<sha512 blob> (66B) = true
  ok   exists n prefix (out of domain) = true
  ok   exists U prefix (out of domain) = true
[sst]                       (after flush + full compaction)
  ok   exists R<blob1> (34B) = true
  ok   exists R<blob3> (34B, absent) = false
  ok   exists R<blob1><repo7> (38B, absent) = false
  ok   exists n prefix (out of domain) = true
  ok   exists U prefix (out of domain) = true
  ok   exists L prefix (out of domain) = true
[prefix_same_as_start]
  ok   psas R<blob1> (34B) = true
  ok   psas R<blob3> (34B, absent) = false
  ok   psas R<blob1><repo7> (38B, absent) = false
  ok   psas n prefix (out of domain) = true
  ok   scan M<repo7> returns 50 = true
  ok   full iteration sees all = true (165/165)
transform experiment: PASS
```

The line that matters most is `exists R<blob1><repo7> (38B, absent but blob IS
referenced) = false`. That is the serving gate: blob 1 is referenced by repos 5
and 9 but not 7. The extractor maps all three to the *same* 34-byte prefix
group, so the bloom filter says "maybe", the seek lands on `R blob1 repo5 …`,
and the `iterate_upper_bound` / `starts_with` check correctly reports false.
Coarse filtering, correct answer.

`RocksEngine` is already written in the way that stays correct here, because
both `scan` and `exists_prefix` set `iterate_upper_bound` from
`prefix_successor` and re-check `starts_with`. **One place is not:** the
`DeletePrefix` fallback in `apply()` for an all-`0xff` prefix uses
`self.db.iterator(IteratorMode::From(prefix, Forward))` with no upper bound. In
this experiment an unbounded seek did *not* truncate at the prefix boundary
(seeking to `M<repo7>` returned 95 further keys past the group), but that is not
a guarantee — with a prefix extractor set and `total_order_seek = false`,
RocksDB is free to stop at the end of the prefix group, and does once the data
spans several SSTs and memtables. That path must set `total_order_seek(true)`
if a prefix extractor is enabled. (In practice no summ prefix is all-`0xff`,
so this is dead code today — but it is the kind of dead code that wakes up.)

### 2.5 What a prefix extractor changes about the DB you already have

- **The extractor name is written into every SST's table properties**
  (`prefix extractor name=` in `rocksdb.aggregated-table-properties`, observed
  as `N/A` for a DB opened without one). Reopening a populated DB with a
  *different* name does not corrupt anything, but the prefix filters in the old
  SSTs are ignored until those files are rewritten by compaction. So version the
  name (`"summ.v1"`) and bump it whenever the key layout changes — silently
  reusing the name after a schema change is the failure mode that yields wrong
  answers, since RocksDB would then trust filters built under the old rules.
- **`ReadOptions::total_order_seek`** becomes meaningful. Every iterator that is
  not a prefix scan must set it. `scan`/`exists_prefix` are prefix scans; the
  catalog scan over `n` is out of domain so it is unaffected; the `apply`
  fallback above is the exception.
- **Point lookups are unaffected** as long as `whole_key_filtering` stays `true`
  (its default). See §4.
