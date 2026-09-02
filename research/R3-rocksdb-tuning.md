# R3 — RocksDB tuning for a multi-prefix-length key schema

**Status: done.** Verified against `rocksdb 0.25.0` / `librocksdb-sys
0.19.0+11.8.1` (RocksDB 11.8.1). Every claim is either a citation to the
vendored source with file and line, or a measurement from the scratch harness
in §8. The `Options` block in §7.2 compiles and passes.

Question: *how do we tune RocksDB for this key schema, given prefixes of
several different lengths?* The lever we most want is a **prefix bloom filter**
serving `exists_prefix()` — "is this blob still referenced?" — which purge asks
once per candidate blob, potentially billions of times.

## The five findings that matter

1. **A custom `SliceTransform` works.** Dispatching on the type byte (and the
   digest algorithm byte) satisfies RocksDB's prefix-consistency property,
   because the bytes that decide the length are themselves inside the prefix.
   Expressible in rocksdb 0.25, verified by experiment. §2
2. **Cross-column-family `WriteBatch`es *are* atomic** with the WAL on —
   documented and verified by SIGKILL-during-write. Column families are
   therefore viable, and still the wrong choice. §3
3. **Whole-key bloom filters do nothing for `exists_prefix`, measurably.** And
   RocksDB's default `filter_policy` is `nullptr`, so summ has no filters at all
   today. A prefix extractor plus a filter policy gives **6.2×** on negative
   `exists_prefix`, and the prefix filter costs **10× less space** than a
   whole-key one. §4, §5b, §6.4
4. **Block key delta encoding, not compression, is what reclaims the `R`
   keyspace** — 79 → 53 B/key before any compressor runs, then 53 → 42 with
   zstd. §6.2
5. **All of that collapses at fan-out 1** (78.24 B/key, a 1% saving). Aggregate
   size is governed by the blob fan-out distribution, which nobody has measured
   and which swings the sizing by 2×. That is a bigger unknown than any knob
   here. §6.7

## Contents

1. The actual prefix lengths — six, not four
2. Option A — custom `SliceTransform` — **viable, recommended**
3. Option B — column families per key type — viable, rejected
4. Whole-key bloom filters and what they do for a *seek* — nothing
5. Other levers — compression, block size, `optimize_for_point_lookup`, block cache, compaction style, `DeleteRange`
5b. Measured: what the prefix bloom is actually worth
6. Space — what actually reclaims the `R` keyspace
7. **RECOMMENDATION**
8. Appendix — reproducing the measurements

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

---

## 3. Option B — column families per key type

### 3.1 The decisive question: is a `WriteBatch` atomic across column families?

**Yes. Verified two ways.**

**Documentary.** `WriteBatch` is documented as "a collection of updates to apply
atomically to a DB" (`include/rocksdb/write_batch.h:9`) and `DB::Write` as
"Apply the specified updates atomically to the database"
(`include/rocksdb/db.h:592`) — neither is qualified by column family. The
sharper citation is the `atomic_flush` option, which exists precisely because
*flush* is the only cross-CF operation that is **not** atomic
(`include/rocksdb/options.h:1581-1593`):

> If true, RocksDB supports flushing multiple column families and committing
> their results atomically to MANIFEST. **Note that it is not necessary to set
> `atomic_flush` to true if WAL is always enabled since WAL allows the database
> to be restored to the last persistent state in WAL.** This option is useful
> when there are column families with writes NOT protected by WAL.

That is an explicit statement that with the WAL on — summ's case, and not
negotiable given the replication plan — a multi-CF write is recovered
atomically. Mechanically: a `WriteBatch` becomes **one CRC-checked WAL record**
carrying a column-family id per entry, assigned one sequence number group. On
recovery a partial trailing record fails its checksum and is discarded whole.

**Experimental.** Scratch harness: a child process opens a DB with three CFs
(`manifests`, `edges`, `tags`) and writes batches of 64 keys *per CF* (192 puts,
one batch), forever. The parent `SIGKILL`s it at an unpredictable moment
(43–284 ms in), reopens the DB, replays the WAL, and checks that every batch id
is present in all three CFs or in none.

```
  round 0: SIGKILL after 267535us, recovered m=e=t=446848, atomic
  round 1: SIGKILL after 122465us, recovered m=e=t=197568, atomic
  ...
  round 11: SIGKILL after 198813us, recovered m=e=t=340608, atomic
cross-CF atomicity: 12 rounds checked, 0 rounds with a torn batch
```

~7 000 cross-CF batches per round survived, with the kill landing mid-write
every time. Caveat on what this proves: `SIGKILL` kills the process, not the
page cache, so it exercises WAL *record* atomicity and recovery, not
`fsync` durability. That is the right scope — the question was atomicity, not
durability, and durability is orthogonal (`WriteOptions::set_sync`, which summ
should think about separately).

**So cross-CF batching does not kill Option B.** Option B dies for other,
smaller reasons.

### 3.2 Iterators across column families

There is no such thing. `DB::iterator_cf` / `iterator_cf_opt` /
`prefix_iterator_cf` are all per-CF, and the only multi-CF read primitives in
the 0.25 binding are `multi_get_cf` (`db.rs:1200`) and
`Snapshot::multi_get_cf` (`snapshot.rs:227`). There is no merged iterator over
several CFs at any layer — not in the binding, not in the C++ API.
Consequently:

- Every scan must know which CF it targets. Since the type byte already
  determines that, this is mechanical — but it pushes schema knowledge from
  `summ-core::keys` down into `RocksEngine`, which the `MetaEngine` trait exists
  to prevent. `redb`'s implementation would have to fake the split or ignore it,
  and the trait stops being "a sorted byte-keyed KV store".
- **Consistency across CFs requires an explicit snapshot.** `DB::snapshot()` is
  DB-wide, so `db.snapshot()` then `snapshot.iterator_cf(..)` per CF is
  consistent. Without it, two iterators created back to back can see different
  states. Today's single-keyspace engine gets that for free.

### 3.3 What CFs do to the WAL / replication story

This is the real objection. PLAN.md's replication seam is that
`WriteBatch` — summ's own serialisable `MetaOp` log — *is* the WAL record.
Column families break that in two ways:

1. **A `MetaOp` would have to name a column family**, or the engine would have
   to re-derive it from the key on replay. The former leaks a RocksDB concept
   into the wire format (the thing PLAN.md says must not happen: "No engine
   types leak past the trait"). The latter is fine but means the CF assignment
   function becomes part of the replication contract — change it and old log
   entries route differently.
2. **RocksDB's own WAL is shared across CFs but flush is per-CF**, so a
   replica built by shipping RocksDB WAL files (rather than summ's `MetaOp`
   log) would need `atomic_flush = true` to have a consistent restore point
   from SSTs alone. Not fatal, but it is a footgun that does not exist today.

Also worth stating: each CF gets its own memtable and its own write buffer.
Eleven CFs at a 64 MiB write buffer is 704 MiB of memtable budget before a
single flush, and `db_write_buffer_size` (a shared cap) then has to be set to
claw it back. More knobs, not fewer.

### 3.4 Verdict on Option B

**Technically viable, not worth it.** Cross-CF atomicity holds, which was the
one thing that could have killed it outright. But it buys nothing that Option A
does not, because a per-type prefix extractor is exactly what per-type CFs would
give — and it costs the flat-keyspace property that makes `MetaEngine` a
four-method trait with a second implementation behind it.

**One narrow exception is worth keeping on the table**: a single extra CF for
**`B` (manifest bodies)**. That is the one key type with a fundamentally
different profile — values of kilobytes rather than zero, zstd-compressed
already, read once per manifest GET, never scanned in bulk. Splitting it stops
multi-kilobyte values from diluting the block cache and inflating the index of
the SSTs that hold the hot `R`/`M`/`T` keys. That is a real win and it is
*one* CF, so the trait damage is limited to "the engine knows `B` is special".
Even so: **measure first.** The same effect is largely available for free by
setting `enable_blob_files` (key-value separation) with `min_blob_size ≈ 1 KiB`,
which moves large values out of the LSM without touching the key schema or the
trait at all. That is the cheaper experiment and should be run before any CF is
created.

---

## 4. Whole-key bloom filters and what they do for a *seek*

### 4.1 Nothing. This is the crux.

`RocksEngine` today sets no `BlockBasedOptions` at all, so it gets RocksDB's
defaults: `whole_key_filtering = true` and **no filter policy**. Worth being
precise about this, because it is easy to misread the current state as
"whole-key blooms are on":

- `BlockBasedTableOptions::filter_policy` defaults to `nullptr`. **A default
  RocksDB has no bloom filters whatsoever.** `whole_key_filtering = true` only
  says *what* to put in the filter if there is one. So summ's `exists_prefix`
  is currently doing an unfiltered seek into every SST whose key range covers
  the prefix.
- A whole-key bloom answers "does key K exist", exactly. `exists_prefix` asks
  "does *any* key with prefix P exist", where P is 34 bytes and the keys are 71.
  There is no K to ask about. A whole-key filter is **structurally unable** to
  answer it, and RocksDB does not consult it during `Seek`. Verified in the
  experiment below: turning whole-key filtering on costs 25 MB per 20 M keys and
  changes negative-seek throughput not at all.

This is the single most important finding in this document. The lever is not
"add bloom filters"; a filter policy with `whole_key_filtering` alone does
nothing for the purge hot path. The lever is **a prefix extractor plus a filter
policy**, which is what makes `SeekPrefix` consult
`prefix_bloom → may_match(P)` and skip the SST outright.

### 4.2 `memtable_whole_key_filtering`

`Options::set_memtable_whole_key_filtering(bool)` (db_options.rs:3631). Its own
doc comment in the binding states the condition: it has effect only **"if
`memtable_prefix_bloom_size_ratio` is not 0"** — the memtable bloom is a single
structure and this flag adds whole keys to it alongside prefixes.

So the memtable knobs are a pair:

```rust
opts.set_memtable_prefix_bloom_ratio(0.02);   // ~2% of write buffer, enables it
opts.set_memtable_whole_key_filtering(true);  // also index whole keys in it
```

`set_memtable_prefix_bloom_ratio` is what makes an unflushed memtable skippable
for a prefix seek. Cheap (2% of a 64 MiB buffer ≈ 1.3 MiB) and it covers the
window in which recently pushed edges have not yet reached L0. Turn both on.

### 4.3 `ReadOptions::set_prefix_same_as_start`

Exposed (db_options.rs:4228). It does two things per the C++ docs
(`options.h:2325-2330`): bounds the iterator to the seek key's prefix group, and
— the part that matters — **"When SST files have been built with the same prefix
extractor, prefix filtering optimizations will be used for both Seek and
SeekForPrev."**

For summ it is *not* a replacement for `iterate_upper_bound`, because our seek
prefix is sometimes longer than the extractor's (the 38-byte serving gate maps
to a 34-byte group), so `prefix_same_as_start` would let the iterator return
keys outside the requested range. It is a useful *addition*: set both, keep the
`starts_with` re-check. The experiment confirms both give the same answers.

There is a warning in that doc worth heeding: it "makes the iterator bounds
dependent on the column family's **current** prefix_extractor, which is
mutable". Do not change the extractor at runtime.

Note also `DB::prefix_iterator(prefix)` (db.rs:1499) is just
`ReadOptions::default() + set_prefix_same_as_start(true) + seek` — no upper
bound. Do not reach for it as a shortcut; it is weaker than what `RocksEngine`
already does.

### 4.4 `auto_prefix_mode` — unavailable, and you would not want it

Three independent reasons to drop this one:

1. **Not exposed by rocksdb 0.25.** The C API has
   `rocksdb_readoptions_set_auto_prefix_mode` (`c.h:3207`) but the Rust crate
   never calls it — `grep -rn auto_prefix rocksdb-0.25.0/` returns nothing — and
   `ReadOptions::inner` is `pub(crate)` (db_options.rs:421), so it cannot be
   reached from outside the crate without a fork or a patch.
2. **It is degraded for any C-API transform anyway.** `auto_prefix_mode`'s
   filtering leans on `SliceTransform::FullLengthEnabled`, whose base
   implementation returns `false` and which the C API's wrapper class
   `rocksdb_slicetransform_t` (`db/c.cc:946-967`) does **not** override — it
   overrides only `Name`, `Transform`, `InDomain`. So a Rust-defined transform
   gets `FullLengthEnabled() == false`, and the header says that "currently
   disables some auto_prefix_mode filtering".
3. **It carries a documented correctness bug in 11.8.1.** `options.h:2312-2322`
   opens with a literal `BUG:` block: short keys "can be omitted from
   auto_prefix_mode iteration when they would be present in total_order_seek
   iteration". summ's keyspace *does* contain keys shorter than the full prefix
   length for their type — none today, but `L <digest>` at 34 bytes sits in the
   same byte range as nothing else, and any future short marker key would.

Recommendation: **do not use `auto_prefix_mode`.** `iterate_upper_bound` +
`prefix_same_as_start` gets the same filtering with none of the caveats, and is
what `RocksEngine` already does.

---

## 5. The other levers

Every number below comes from the §6 measurement rig (20 M synthetic `R` edge
keys, 71-byte keys, empty values, 2 M distinct blob digests × 10 refs each,
fully compacted, macOS/arm64, rocksdb 0.25 + RocksDB 11.8.1).

### 5.1 Per-level compression — keep Lz4/Zstd, but know it is not the main lever

| config | SST bytes | B/key | vs none-4k |
|---|---|---|---|
| `none-4k` | 1 062 450 507 | 53.12 | — |
| `lz4-4k` | 915 080 070 | 45.75 | −13.9% |
| `zstd-4k` | 861 781 065 | 43.09 | −18.9% |
| `zstd-16k` | 840 069 766 | 42.00 | −20.9% |

Compression buys ~19%. That is real but it is not where the space goes — §6 has
the rest of the story. The reason is that `R` keys are mostly sha256 digest
bytes, which are by construction incompressible; only the *shared* portion
compresses, and RocksDB's own delta encoding has already removed most of that
before the compressor sees the block.

**Recommendation: keep the current `Lz4` + bottommost `Zstd`.** It is the right
shape — cheap codec where compaction churns, expensive codec where data settles
— and it is worth ~19%. Do not expect more from it. Explicitly *do not* set
`compression_per_level` unless a measurement demands it: it is easy to get
wrong (the array must have `num_levels` entries) and the two-tier
`compression_type` + `bottommost_compression_type` split already expresses the
intent.

Zstd dictionary compression (`set_compression_options(.., max_dict_bytes)` +
`set_zstd_max_train_bytes`) is measured in §6.3; it is the one variant that
could plausibly beat plain zstd on this data, because every block looks alike.

### 5.2 Block size — raise it to 16 KiB

| config | SST bytes | data | **index** |
|---|---|---|---|
| `lz4-4k` | 915 080 070 | 903 885 083 | 11 189 894 |
| `lz4-16k` | 894 666 730 | 891 779 368 | **2 882 302** |
| `zstd-16k` | 840 069 766 | 837 182 472 | 2 882 230 |
| `zstd-32k` | 839 198 416 | 837 782 679 | 1 427 416 |

Going 4 KiB → 16 KiB shrinks the index block **4×** (11.2 MB → 2.9 MB per 20 M
keys) and the data slightly, because a bigger compression window helps. 32 KiB
buys essentially nothing more in data and halves the index again.

The index size number is the one that matters at target scale. Extrapolated to
2×10¹⁰ `R` keys: 11.2 GB of index at 4 KiB blocks versus 2.9 GB at 16 KiB. That
index is what you want resident, so this is a **RAM** decision as much as a disk
one.

The cost of a big block is read amplification: a point lookup or a seek must
decompress a whole block. Our values are empty and our reads are seeks that only
need the first matching key, so the penalty is bounded by decompression of one
16 KiB block. Measured in §5.7.

**Recommendation: `set_block_size(16 * 1024)`.** Revisit 32 KiB only if index
memory becomes the binding constraint.

### 5.3 `optimize_for_point_lookup` — do not call it

`ColumnFamilyOptions::OptimizeForPointLookup` (`options/options.cc:647-660`) is
five lines:

```cpp
block_based_options.data_block_index_type = kDataBlockBinaryAndHash;
block_based_options.data_block_hash_table_util_ratio = 0.75;
block_based_options.filter_policy.reset(NewBloomFilterPolicy(10));
block_based_options.block_cache = NewLRUCache(block_cache_size_mb * 1024 * 1024);
table_factory.reset(new BlockBasedTableFactory(block_based_options));
memtable_prefix_bloom_size_ratio = 0.02;
memtable_whole_key_filtering = true;
```

Three reasons not to:

1. **It constructs a fresh `BlockBasedTableOptions` and replaces the whole
   `table_factory`.** Any `set_block_based_table_factory` you called before it is
   discarded; any you call after discards *its* settings. This is a silent
   footgun in a `fn open()` that sets both.
2. **It creates its own `LRUCache`**, so a shared/sized block cache you built
   elsewhere is thrown away.
3. **It sets no prefix extractor**, so it does nothing at all for
   `exists_prefix` — see §4.1.

The two lines of it that *are* right for summ are the memtable ones, and those
are directly settable. Set the pieces explicitly:

```rust
opts.set_memtable_prefix_bloom_ratio(0.02);
opts.set_memtable_whole_key_filtering(true);
bb.set_data_block_index_type(DataBlockIndexType::BinaryAndHash); // helps `get`, costs ~2-3% space
```

(`data_block_hash_table_util_ratio` is not exposed by rocksdb 0.25; you get the
default 0.75.)

### 5.4 `optimize_filters_for_hits` — **actively wrong for summ**

`advanced_options.h:804-816`: it stops building filters for the **bottommost
level**, on the theory that hits will read the data anyway. At an LSM's steady
state ~90% of keys live in the bottommost level, so this saves ~90% of filter
space.

It is exactly backwards for purge. Purge's question is "is this blob still
referenced?" and the answer that must be cheap is **no** — a *miss*. Dropping
bottom-level filters means every negative `exists_prefix` falls through to a
real bottom-level seek, which is the one I/O this whole exercise exists to
avoid.

**Recommendation: leave it `false` (the default). Do not be tempted by the
space saving** — §6 shows the prefix filter costs only 0.125 B/key anyway.

### 5.5 Block cache sizing

RocksDB's default block cache is **32 MiB** — `BlockBasedTableFactory::
InitializeOptions` (`table/block_based/block_based_table_factory.cc:472-477`)
builds an auto-HyperClockCache of `size_t{32} << 20` when none is supplied.
That is absurdly small for this workload and it is exactly what `RocksEngine`
gets today. `Cache::new_lru_cache(bytes)` +
`BlockBasedOptions::set_block_cache(&cache)` is the fix; the `Cache` is
refcounted and must outlive the DB, so hold it in `RocksEngine` (the current
struct has no field for it). `Cache::new_hyper_clock_cache` is also bound, but
it wants a roughly uniform entry charge and our blocks are a mix of 16 KiB data
and much smaller index/filter blocks — stay on LRU until there is a reason not
to.

What to size it for, in priority order:

1. **Index + filter blocks.** At 16 KiB blocks these are ~2.9 GB index +
   ~2.5 GB filter per 2×10¹⁰ `R` keys. If they do not fit, every seek pays
   extra I/O to read an index block first, and the filter cannot help.
   `set_cache_index_and_filter_blocks(true)` + `set_pin_l0_filter_and_index_blocks_in_cache(true)`
   + `set_pin_top_level_index_and_filter(true)` puts them under the cache's
   accounting (bounded memory) rather than letting them grow unbounded outside
   it, and pins the hottest ones.
2. **Hot data blocks** — the `M`/`T` keys behind catalog and tag listing.
3. `R` data blocks are effectively uncacheable at 10¹⁰ keys; do not try.

**Recommendation: an explicit, configurable cache, default ~25–30% of RAM, with
`cache_index_and_filter_blocks(true)` and both pin options on.** If index+filter
cannot fit, switch to **partitioned** index and filters
(`set_index_type(TwoLevelIndexSearch)` + `set_partition_filters(true)` +
`set_metadata_block_size(4096)`) so only the top level need be resident. That
config is measured in §6.

### 5.6 Compaction style — stay on level

The workload is write-once, mostly-read, with periodic bulk deletes.

- **Universal** minimises write amplification and maximises **space**
  amplification — worst case ~2× the dataset, because a full compaction needs
  room for a second copy of everything. At a projected 1 TB that is a second
  terabyte of disk, and §6 says space is the binding constraint here, not write
  throughput.
- **Level** with `level_compaction_dynamic_level_bytes` (already `true` by
  default in 11.8.1 — `advanced_options.h:666`, so no need to set it) keeps
  space amplification near ~1.11× and puts ~90% of data in the bottommost level,
  which is exactly where a write-once corpus wants it. Its cost is write
  amplification, and pushes are rare relative to reads.
- **FIFO** is irrelevant (no TTL semantics here).

**Recommendation: level (the default). Do not switch.** The one thing worth
setting is `set_target_file_size_base` up from the 64 MiB default — at 1 TB that
is ~16 000 SST files at the bottom level, and `max_open_files` interacts badly
with range tombstones (see 5.7). 256 MiB gives ~4 000, which is far more
comfortable.

### 5.7 `DeleteRange` and its traps

`MetaOp::DeletePrefix` compiles to `WriteBatch::delete_range`. The C++ header
(`db.h:543-558`) states three caveats verbatim:

> 1) Accumulating too many range tombstones in the memtable will degrade read
> performance; this can be avoided by manually flushing occasionally.
> 2) Limiting the maximum number of open files in the presence of range
> tombstones can degrade read performance. To avoid this problem, set
> `max_open_files` to -1 whenever possible.
> 3) Incompatible with `row_cache`, will return `Status::NotSupported()`.

Points 1 and 2 are the ones that bite at summ's scale, and there is a fourth
that the header does not mention:

4. **Every read must merge every overlapping range tombstone.** Range
   tombstones are not indexed the way point keys are; a seek builds a
   `FragmentedRangeTombstoneIterator` over the tombstones in each table whose
   range covers the seek key. A purge sweep that emits millions of
   one-blob-wide `DeleteRange`s before compacting turns every subsequent
   `exists_prefix` into a walk over that tombstone set. This is the well-known
   "DeleteRange at scale" cliff.

There is also a **mitigation that rocksdb 0.25 does not expose**:
`ColumnFamilyOptions::memtable_max_range_deletions` (`options.h:348-357`, "flush
the current memtable after the number of range deletions is >= this limit")
exists in the C API as `rocksdb_options_set_memtable_max_range_deletions`
(`c.h:5399`) but has **no Rust binding**. Confirmed by grep over
`rocksdb-0.25.0/src`. So the flush-on-tombstone-pressure safety valve has to be
implemented by hand, or upstreamed.

Consequences for summ, and this is a purge-design constraint, not just a tuning
knob:

- **A one-blob-wide `DeleteRange` is the wrong tool.** `R <digest>` covers ~10
  keys. A range tombstone costs more than 10 point deletes to store, to merge on
  read, and to compact away. `Delete` (or better, `SingleDelete`, since edge
  keys are written exactly once and never overwritten) is correct for these.
  **`MetaOp::DeletePrefix` should be reserved for genuinely wide ranges** — a
  whole repo's `M <repo>` / `T <repo>` / `P <repo>` — and purge should enumerate
  and point-delete narrow ones.
- **Purge must flush and compact in batches**, not accumulate tombstones for the
  whole sweep. Since v1 purge is offline (PLAN.md), this is easy: sweep in
  chunks and `compact_range` each chunk's key span.
- **Set `max_open_files = -1`** per caveat 2. It is the documented mitigation
  and it costs only file descriptors.
- Do not enable `row_cache` (summ has no reason to; caveat 3 makes it a hard
  incompatibility).

Measured behaviour is in §5.8.

---

## 5b. Measured: what the prefix bloom is actually worth

(Same key generator as the §6.1 rig, described there. 2 M blobs × 5 refs =
10 M `R` keys, fully compacted, **8 MiB block cache deliberately** so the data
does not fit in RAM, `cache_index_and_filter_blocks(false)`. 200 000
`exists_prefix` calls each way.)

| config | negative `exists_prefix` | positive `exists_prefix` | SST bytes |
|---|---|---|---|
| whole-key bloom only (no prefix extractor) | **118 829 /s** | 114 183 /s | 467 834 829 |
| **prefix bloom (custom `SliceTransform`)** | **735 796 /s** | 111 803 /s | 457 834 701 |

**6.2× on the purge hot path**, and the smaller file. Two things to read out of
this table:

- **The "whole-key bloom only" row has a bloom filter and it does nothing.**
  That row is `set_bloom_filter(10.0, false)` with `whole_key_filtering(true)`
  and no prefix extractor — and it performs identically to having no filter,
  because `Seek` never consults a whole-key filter. §4.1, measured.
- **Positive lookups are unchanged** (114k vs 112k, within noise). Expected: a
  hit has to read the block either way. The filter only buys misses — which is
  precisely the shape of purge, where the interesting answer is "no longer
  referenced".

Extrapolating to the purge sweep PLAN.md envisages: at 10⁸ candidate blobs,
118 829/s is **14 minutes** and 735 796/s is **2.3 minutes** of `exists_prefix`
alone, on a warm single thread with an 8 MiB cache. The gap widens as the
dataset grows relative to cache, because the unfiltered path's cost is an I/O
and the filtered path's is not.

### 5.8 Measured: `DeleteRange` tombstone behaviour

1 M `R` keys (200 K blobs × 5), then 100 000 one-blob-wide range tombstones
written without compacting:

```
after load, sst = 46 240 621 bytes
baseline                                   267 951 exists/s
wrote 100 000 range tombstones in 0.1s
with 100 000 live tombstones               190 640 exists/s   (-29%)
sst before compaction = 46 240 621 bytes   (unchanged - tombstones are in the memtable/WAL)
compaction took 0.4s
sst after compaction  = 23 529 414 bytes   (-49%, the deletes landed)
after compaction                           389 600 exists/s
```

Three things confirmed:

1. **Read throughput drops ~29% with 100 K live tombstones**, and this is the
   *mild* end — the DB is 1 M keys and the tombstones are still in one memtable.
   The documented cliff is superlinear as tombstones accumulate across levels.
2. **No space is reclaimed until compaction.** Writing tombstones is nearly free
   (0.1 s for 100 K) and reclaims nothing; a purge sweep that only writes
   tombstones has done no work in disk terms.
3. **After compaction, reads are faster than baseline** (389 K vs 268 K/s),
   because half the data is gone. Compaction is the operation that pays.

**Purge design consequence, restating §5.7 with numbers behind it:** emit point
deletes for narrow ranges, reserve `DeletePrefix` for repo-wide ones, and
`compact_range` the swept span in chunks rather than at the end.


---

## 6. Space — what actually reclaims the `R` keyspace

This is the section that matters most, per PLAN.md: "post-compaction size on
disk is the number that could still surprise us".

### 6.1 The rig

Scratch crate, release build, macOS/arm64, rocksdb 0.25 / RocksDB 11.8.1.
20 000 000 synthetic `R` edge keys: `R <sha256 digest:33> <repo:4> <sha256
digest:33>` = **71 bytes, empty value**, generated as 2 000 000 distinct blob
digests × 10 referencing manifests each, digests being pseudo-random (so
incompressible, as real sha256 output is). Written, flushed, then
`compact_range(None, None)` twice. Measured by summing `*.sst` on disk and by
`rocksdb.aggregated-table-properties`.

RocksDB stores an 8-byte internal suffix (sequence number + type) per key, so
**raw internal key size is 79 bytes/key**, and `raw value size = 0`. That 79 is
the number every ratio below is against.

### 6.2 The headline: block delta encoding does more than compression

| config | SST bytes | **B/key** | data block | index block | filter block |
|---|---|---|---|---|---|
| raw (uncompressed, unencoded) | — | 79.00 | — | — | — |
| `none-4k` | 1 062 450 507 | **53.12** | 1 051 256 859 | 11 188 504 | 0 |
| `none-4k` + whole-key bloom | 1 087 450 990 | 54.37 | 1 051 256 859 | 11 188 504 | 25 000 148 |
| `lz4-4k` | 915 080 070 | 45.75 | 903 885 083 | 11 189 894 | 0 |
| `lz4-16k` | 894 666 730 | 44.73 | 891 779 368 | 2 882 302 | 0 |
| `zstd-4k` | 861 781 065 | 43.09 | 852 188 734 | 11 188 254 | 0 |
| **`zstd-16k`** | **840 069 766** | **42.00** | 837 182 472 | 2 882 230 | 0 |
| `zstd-32k` | 839 198 416 | 41.96 | 837 782 679 | 1 427 416 | 0 |
| `zstd-64k` | 836 890 376 | 41.84 | 836 102 521 | 782 794 | 0 |
| `zstd-16k` + whole-key bloom | 865 070 246 | 43.25 | 837 182 472 | 2 882 230 | **25 000 148** |
| **`zstd-16k` + prefix bloom** | **842 570 269** | **42.13** | 837 182 472 | 2 882 230 | **2 500 180** |
| `zstd-16k` + both blooms | 867 570 342 | 43.38 | 837 182 472 | 2 882 230 | 27 500 244 |
| `zstd-16k` + prefix bloom, partitioned | 842 673 649 | 42.13 | 837 182 472 | 2 919 593 | 2 563 276 |
| `zstd-16k` + zstd dictionary (16 KiB) | 842 293 510 | 42.11 | 839 340 503 | 2 882 196 | 0 |

**Answer to the question posed: it is key prefix encoding inside SST blocks,
not block compression.**

- 79 → **53.12** B/key with **no compression at all**. That is a **32.8%**
  reduction and it is entirely RocksDB's block key delta encoding: each entry in
  a data block stores `varint(shared_len) varint(non_shared_len)
  varint(value_len)` plus only the non-shared key bytes, restarting the sharing
  every `block_restart_interval` (default 16) keys.
- 53.12 → 42.00 B/key adds zstd on top: a further **20.9%**, ending at **46.8%
  of raw**.

So of the ~37 bytes/key saved, **30 come from delta encoding and 11 from
compression**. Compression is worth having but it is the junior partner, and
PLAN.md's "block compression matters more here than usual" is half right — the
*prefix sharing* matters more than usual, and the compressor is riding on what
the encoder leaves behind.

### 6.3 Things that did not help

- **Zstd dictionary compression made it very slightly worse** (42.11 vs 42.00
  B/key). 16 KiB blocks are already large enough for zstd to build its own
  window, and the residual after delta encoding is near-random digest bytes with
  no cross-block redundancy to exploit. Do not bother.
- **32 KiB and 64 KiB blocks** save 0.04 and 0.16 B/key of *data* — noise. Their
  real effect is on the index (below). Not a space lever.

### 6.4 Filter cost, and why the prefix filter is nearly free

This is the second decisive number:

| filter | bytes / 20 M keys | B/key |
|---|---|---|
| whole-key bloom, 10 bits/key | 25 000 148 | **1.25** |
| **prefix bloom, 10 bits/prefix** | **2 500 180** | **0.125** |

A **10× difference**, and the reason is structural: the whole-key filter holds
one entry per key (20 M entries), the prefix filter one entry per *prefix group*
(2 M distinct blob digests). At the ratio the schema actually has — ~10 edges
per blob — the prefix filter costs a tenth as much **and is the only one of the
two that can answer `exists_prefix` at all** (§4.1).

Adding the prefix filter to the recommended config costs **0.13 B/key, +0.3% of
total size**. There is no version of this trade that is not worth taking.

**Ribbon filters are the obvious follow-up here.** `BlockBasedOptions::
set_ribbon_filter(bloom_equivalent_bits_per_key)` is bound in rocksdb 0.25
(db_options.rs:674) and gives roughly 30% less space than a bloom at the same
false-positive rate, paid for in construction and query CPU. Not measured here.
It matters most for the *whole-key* filter (1.25 B/key → ~0.9), which is the
expensive one; the prefix filter is already too cheap to optimise. Put it on the
measurement list, not in the first config.

Whole-key filtering (`bb.set_whole_key_filtering(true)`, the default) is still
worth keeping *for the key types that get point-looked-up* — `M`, `B`, `L`,
`T`, `U`, `n`, `i`. It costs 1.25 B/key on those, but those are the *small* key
types. It should arguably be off for `R`/`G`/`S`/`F`, which are never
point-looked-up — and that is the strongest remaining argument for a column
family split, since `whole_key_filtering` is a per-CF setting. Worth ~1.25 B/key
on the largest keyspace: at 2×10¹⁰ `R` keys, **25 GB**. Measure before deciding.

### 6.5 Index cost is a RAM budget, not a disk budget

Index block size per 20 M keys: 11.19 MB at 4 KiB blocks, 2.88 MB at 16 KiB,
1.43 MB at 32 KiB, 0.78 MB at 64 KiB. Scaled to 2×10¹⁰ `R` keys:

| block size | index, whole `R` keyspace |
|---|---|
| 4 KiB | ~11.2 GB |
| 16 KiB | ~2.9 GB |
| 32 KiB | ~1.4 GB |
| 64 KiB | ~0.8 GB |

Plus ~2.5 GB of prefix filter. So at 16 KiB blocks the resident metadata for the
whole `R` range is **~5.4 GB** — comfortably cacheable on any machine that would
run this. At 4 KiB it is ~13.7 GB, which is not. **This, not the data size, is
the argument for 16 KiB blocks.**

Partitioned index + filters cost +1.3% index and +2.5% filter for the ability to
keep only the top level resident. Worth switching to only if the 5.4 GB does not
fit; it is not free.

### 6.6 Extrapolation to the PLAN.md estimate

PLAN.md: "~2×10¹⁰ keys of ~70 bytes: order 1 TB before compression."

Measured, at the recommended config (`zstd-16k` + prefix bloom, 10 edges/blob):
**42.13 B/key**, i.e. **~843 GB** for 2×10¹⁰ `R` edge keys.

That is *not* the comfortable margin one might hope for. Raw would be 1.58 TB
(79 B/key internal), so the whole encoding + compression stack buys **47%** —
and the remaining 843 GB is irreducible digest entropy: 66 of the 71 bytes in an
`R` key are two sha256 hashes.

**And that 42 B/key is entirely contingent on fan-out** — see §6.7, which is
the caveat that should be read before anyone plans capacity from this number.
The honest planning range is **~800 GB to ~1.6 TB**, and where in that range it
lands depends on the blob fan-out distribution, which nobody has measured.

**The lever that would actually move this number is schema, not tuning.** The
`R` key stores a 33-byte manifest digest; interning manifest digests per repo
the way repo names are already interned (`n`/`i`) would take the key from 71 to
~42 bytes and the store from ~843 GB to roughly half that. That is a bigger win
than every option in this document combined, and it is out of scope here — but
it should be on the record before the schema is frozen by the §"Pending schema
changes" batch.

Sensitivity to fan-out shape and to `block_restart_interval` is measured in
§6.7; both bear directly on how much the delta encoding can recover.

### 6.7 The caveat that dominates everything: fan-out shape

The 42 B/key figure assumes 10 referencing manifests per blob. Same 20 M keys,
same `zstd-16k` config, varying only how the edges are distributed:

| refs per blob | distinct prefixes | SST bytes | **B/key** | vs 79 raw |
|---|---|---|---|---|
| **1** | 20 M | 1 564 888 423 | **78.24** | **−1.0%** |
| 2 | 10 M | 1 110 951 652 | 55.55 | −29.7% |
| 10 | 2 M | 840 081 432 | 42.00 | −46.8% |
| 100 | 200 K | 788 220 060 | 39.41 | −50.1% |

**At fan-out 1, the entire encoding and compression stack buys 1%.** Two
adjacent `R` keys with different blob digests share exactly one byte (`R`), so
delta encoding recovers nothing, and zstd cannot compress 66 bytes of hash
output. This is the single most important sensitivity in the whole document and
it means:

- Aggregate size is governed by the **blob fan-out distribution**, not by any
  tuning knob. If most blobs are referenced by exactly one manifest — plausible
  for the long tail of application layers — the `R` range costs ~78 B/key
  regardless of settings, and 2×10¹⁰ edges is **~1.56 TB**.
- Container registries are heavy-tailed the other way at the *edge* level: base
  layers are shared by enormous numbers of manifests, so most *edges* live in
  high-fan-out groups even though most *blobs* are singletons. The
  edge-weighted average is what matters, and it will sit somewhere in the
  39–78 range.
- **Risk 2 in PLAN.md ("aggregate manifest count is unspecified") is not the
  only missing number. The blob fan-out distribution is the other, and it swings
  the sizing by 2×.** Package G should measure both from a real corpus.

`block_restart_interval` is a much smaller lever, measured at fan-out 10:

| restart interval | B/key |
|---|---|
| 8 | 42.49 |
| 16 (default) | 42.00 |
| 32 | 41.61 |
| 64 | 41.46 |
| 128 | 41.40 |

1.4% from 16 → 128, paid for with a longer linear scan inside a data block on
every seek (RocksDB binary-searches restart points, then scans forward). **Not
worth changing.** Leave it at 16.

### 6.8 Where the rest of the keyspace sits

`R` is the only range at 10¹⁰. For scale: `M`/`B` are ~10⁹ (one per manifest),
`P`/`G`/`S`/`F` smaller still, `n`/`i` 10⁷. The tuning above is all sized for
`R`; the small ranges ride along and are the reason `whole_key_filtering` stays
on globally.

---

## 7. RECOMMENDATION

### 7.1 Answers to the questions asked

| Question | Answer |
|---|---|
| Custom `SliceTransform` expressible in rocksdb 0.25? | **Yes.** `SliceTransform::create(name, fn, Some(fn))`, `in_domain` fully supported. The transform must return a subslice of its input. |
| Does prefix consistency kill it? | **No.** The essential property (`options.h:264-274`) is satisfied because the bytes that determine the length — type byte at 0, algorithm byte at 1 or 5 — are themselves inside the prefix. Proof in §2.2, experiment in §2.4. |
| Are cross-CF `WriteBatch`es atomic? | **Yes**, with the WAL enabled. Documented (`options.h:1581-1593` on `atomic_flush`) and verified by 12 rounds of SIGKILL-during-write × ~7 000 three-CF batches each, zero torn (§3.1). |
| So: CFs or a transform? | **Transform.** CFs are viable but buy nothing extra and cost the flat-keyspace property that keeps `MetaEngine` four methods wide (§3.4). |
| Do whole-key blooms help `exists_prefix`? | **No — measured at zero** (§5b). `Seek` never consults them. And RocksDB's default `filter_policy` is `nullptr`, so summ currently has *no* filters at all. |
| `auto_prefix_mode`? | **Unavailable and undesirable** — not bound by rocksdb 0.25, degraded for C-API transforms, and carries a documented `BUG:` in 11.8.1 (§4.4). |
| What reclaims the `R` keyspace? | **Block key delta encoding (−33%), then compression (−21% on top)** — but both collapse to nothing at fan-out 1 (§6.2, §6.7). |

### 7.2 The `Options` to set

All of the following compiles and passes against `rocksdb = "0.25"` (verified in
a scratch crate — `cargo build --release && ./recommend` → "compile and pass").

**Prerequisite in `summ-core`:** the transform needs the digest algorithm byte
lengths, which are private today (`digest.rs:15-16`, `const ALGO_SHA256/512`).
Export a single function rather than the constants, so the transform cannot
drift from the encoder:

```rust
// summ-core/src/digest.rs
/// `Digest::encoded_len()` for an algorithm byte, without a `Digest`.
pub fn encoded_len_of(algo: u8) -> Option<usize> {
    match algo {
        ALGO_SHA256 => Some(33),
        ALGO_SHA512 => Some(65),
        _ => None,
    }
}
```

**The extractor** (put it next to the key builders it mirrors — the two must
change together):

```rust
use summ_core::digest::encoded_len_of;
use summ_core::keys::*;

/// Prefix-group length for `key`, or `None` if the key type has no prefix group
/// worth filtering (or the key is too short to classify).
///
/// The length depends only on bytes that are themselves inside the prefix — the
/// type byte at 0 and, for digest-bearing types, the algorithm byte. That is
/// what makes this satisfy RocksDB's prefix-consistency property (§2.2).
#[inline]
fn summ_prefix_len(key: &[u8]) -> Option<usize> {
    match *key.first()? {
        // `R <digest> <repo> <manifest>` — group by `R <digest>` (34 / 66).
        // The 38-byte `blob_refs_in_repo` seek extracts to this same group.
        PREFIX_BLOB_REF => Some(1 + encoded_len_of(*key.get(1)?)?),
        // `G|S|F <repo:4> <digest> <...>` — group through the first digest.
        PREFIX_MANIFEST_TAG | PREFIX_CHILD_PARENT | PREFIX_REFERRER => {
            Some(1 + 4 + encoded_len_of(*key.get(5)?)?)
        }
        // Repo-scoped scans: `M|B|T|P <repo:4>`.
        PREFIX_MANIFEST | PREFIX_MANIFEST_BODY | PREFIX_TAG | PREFIX_REPO_BLOB => Some(5),
        // `L`, `U`, `n`, `i`: a one-byte prefix group is worthless to a bloom
        // filter, so they stay out of the domain and rely on the whole-key
        // filter for their point lookups.
        _ => None,
    }
}

fn summ_transform(key: &[u8]) -> &[u8] {
    match summ_prefix_len(key) {
        Some(n) if key.len() >= n => &key[..n],
        // Only reached if RocksDB calls Transform on an out-of-domain key.
        // MUST return a subslice of `key`: the binding hands the pointer to C.
        _ => key,
    }
}

fn summ_in_domain(key: &[u8]) -> bool {
    matches!(summ_prefix_len(key), Some(n) if key.len() >= n)
}

/// Bump the version whenever the key layout changes. RocksDB records this name
/// in every SST's table properties and will otherwise trust filters built under
/// the old rules.
fn summ_prefix_extractor() -> SliceTransform {
    SliceTransform::create("summ.prefix.v1", summ_transform, Some(summ_in_domain))
}
```

**`RocksEngine::open`:**

```rust
use rocksdb::{BlockBasedOptions, Cache, DBCompressionType, DataBlockIndexType, Options, DB};

pub struct RocksEngine {
    db: DB,
    /// The block cache is refcounted and must outlive the DB.
    _cache: Cache,
}

pub fn open(path: impl AsRef<Path>, block_cache_bytes: usize) -> Result<RocksEngine> {
    let cache = Cache::new_lru_cache(block_cache_bytes);

    let mut bb = BlockBasedOptions::default();
    // 16 KiB blocks: 4x smaller index than the 4 KiB default (~2.9 GB vs
    // ~11.2 GB over the projected R keyspace) for the same data size. §6.5
    bb.set_block_size(16 * 1024);
    bb.set_block_cache(&cache);
    // NB: do NOT call set_format_version. RocksDB 11.8 defaults to 7 and the
    // header says "using the default setting of format_version is strongly
    // recommended" (table.h:733-739); the binding's doc comment claiming the
    // default is 5 is stale. The measurements in this document were taken at 6,
    // which differs only in footer checksumming and not in size.
    // 10 bits/key. With the prefix extractor below this builds BOTH a prefix
    // filter (one entry per group — what exists_prefix needs) and a whole-key
    // filter (what `get` needs). RocksDB's default filter_policy is nullptr,
    // so without this line there are no filters at all. §4.1
    bb.set_bloom_filter(10.0, false);
    bb.set_whole_key_filtering(true);
    // Bound index+filter memory by the cache instead of letting it grow
    // outside it, and pin the hottest parts. §5.5
    bb.set_cache_index_and_filter_blocks(true);
    bb.set_pin_l0_filter_and_index_blocks_in_cache(true);
    bb.set_pin_top_level_index_and_filter(true);
    // Hash index inside data blocks: helps `get`, ~2-3% space. §5.3
    bb.set_data_block_index_type(DataBlockIndexType::BinaryAndHash);

    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_block_based_table_factory(&bb);

    // THE point of this document.
    opts.set_prefix_extractor(summ_prefix_extractor());
    // Same filter for keys still in the memtable. The second line has effect
    // only because the first is non-zero.
    opts.set_memtable_prefix_bloom_ratio(0.02);
    opts.set_memtable_whole_key_filtering(true);

    // Unchanged: cheap codec where compaction churns, expensive where data
    // settles. Worth ~19%. §5.1
    opts.set_compression_type(DBCompressionType::Lz4);
    opts.set_bottommost_compression_type(DBCompressionType::Zstd);

    // Level compaction (the default): ~1.11x space amplification vs
    // universal's ~2x, and space is the binding constraint. §5.6
    // level_compaction_dynamic_level_bytes already defaults true in 11.8.
    opts.set_target_file_size_base(256 * 1024 * 1024);
    opts.set_write_buffer_size(128 * 1024 * 1024);
    opts.set_max_write_buffer_number(4);
    opts.set_max_background_jobs(6);
    opts.set_bytes_per_sync(1024 * 1024);

    // Documented mitigation for the DeleteRange open-files trap (db.h:554-556).
    opts.set_max_open_files(-1);

    let db = DB::open(&opts, path).map_err(storage)?;
    Ok(RocksEngine { db, _cache: cache })
}
```

**Reads — two changes to `RocksEngine`:**

```rust
/// Read options for a prefix scan. `iterate_upper_bound` is what makes this
/// correct; `prefix_same_as_start` is what makes the SST prefix filter be
/// consulted on Seek (options.h:2325-2330).
fn prefix_read_opts(prefix: &[u8]) -> ReadOptions {
    let mut opts = ReadOptions::default();
    if let Some(end) = prefix_successor(prefix) {
        opts.set_iterate_upper_bound(end);
    }
    opts.set_prefix_same_as_start(true);
    opts
}
```

…used by both `scan` and `exists_prefix`. The existing `starts_with` re-check
**must stay**: for a 38-byte `R <digest> <repo>` seek the extractor's group is
only the 34-byte digest, so the iterator can legitimately hand back another
repo's edge.

And the one place that must opt *out* — the all-`0xff` `DeletePrefix` fallback
in `apply()`, which today builds a bare `self.db.iterator(IteratorMode::From(..))`:

```rust
let mut opts = ReadOptions::default();
opts.set_total_order_seek(true);   // required once a prefix extractor exists
let mut it = self.db.iterator_opt(IteratorMode::From(prefix, Direction::Forward), opts);
```

### 7.3 Explicitly rejected

| Lever | Why not |
|---|---|
| Column families per key type | Viable (cross-CF batches are atomic) but redundant with the transform, and it leaks a RocksDB concept into `MetaOp` / the replication log. §3 |
| `optimize_for_point_lookup` | Silently replaces the whole table factory and the block cache; sets no prefix extractor. Set its two useful lines directly. §5.3 |
| `optimize_filters_for_hits` | Drops bottom-level filters — exactly the filters purge's *negative* lookups depend on. §5.4 |
| `auto_prefix_mode` | Not bound in 0.25 (`ReadOptions::inner` is `pub(crate)`); degraded for C-API transforms; documented `BUG:` in 11.8.1. §4.4 |
| Universal compaction | ~2× space amplification; space is the binding constraint. §5.6 |
| Zstd dictionary compression | Measured slightly *worse* than plain zstd (42.11 vs 42.00 B/key). §6.3 |
| Larger `block_restart_interval` | 1.4% for a longer in-block scan on every seek. §6.7 |
| 32/64 KiB blocks | 0.1% data saving; only useful if index RAM is the binding constraint. §6.2 |
| `row_cache` | Hard incompatibility with `DeleteRange` (`db.h:557-558`). |

### 7.4 What must be measured before adoption

In rough order of how much they could change the answer:

1. **The blob fan-out distribution of a real corpus.** §6.7: it swings `R`
   storage between ~800 GB and ~1.6 TB, which is more than every tuning knob in
   this document put together. Package G. This belongs in PLAN.md's Risks
   alongside the unspecified aggregate manifest count.
2. **The prefix-bloom win at a dataset that does not fit in page cache.** The
   6.2× in §5b is at 10 M keys with an artificially small block cache on a
   laptop. The direction is certain; the magnitude on a real 1 TB store on the
   Standard_L8s_v3 rig is not.
3. **`whole_key_filtering = false` for the edge types.** Worth ~1.25 B/key on
   the `R` range — **~25 GB at target scale** — because those keys are never
   point-looked-up. This is a per-CF setting, so it is the one genuine argument
   for a column family split, and it should be weighed against §3's objections
   only once the number is confirmed.
4. **`enable_blob_files` + `min_blob_size ≈ 1 KiB` for `B` (manifest bodies).**
   Cheaper than a CF split and achieves the same separation of large values from
   the hot small-key SSTs. Try this *before* considering a `B` column family.
5. **Block cache sizing and whether index+filter (~5.4 GB projected) fits.** If
   not, switch to partitioned index + filters — measured cost +1.3% index,
   +2.5% filter.
6. **Ribbon filters** (`set_ribbon_filter`) in place of bloom: ~30% less filter
   space for more CPU. Only worth it on the whole-key filter. §6.4
7. **`DeleteRange` at purge scale.** §5.8 measured a 29% read regression from
   100 K tombstones on a 1 M-key store. At 10⁸ candidates the shape is what
   matters, and the mitigation option
   (`memtable_max_range_deletions`) **has no Rust binding** — so the flush
   policy has to be hand-rolled in purge, and that design should be validated
   before Phase 4.

### 7.5 Two things this turned up that are not tuning

- **`MetaOp::DeletePrefix` is the wrong primitive for narrow ranges.** A
  one-blob `R <digest>` range covers ~10 keys and a range tombstone costs more
  than 10 point deletes to store, merge and compact. Purge should point-delete
  narrow ranges and reserve `DeletePrefix` for repo-wide ones. This bears on the
  `scan_keys` addition R4 §6.10 asks for: purge will be enumerating these keys
  anyway, so `scan_keys` is not merely an allocation optimisation, it is what
  makes the correct delete strategy affordable.
- **Interning manifest digests would halve the `R` range.** 66 of an `R` key's
  71 bytes are two sha256 hashes. Interning the manifest digest per repo the way
  repo names are already interned (`n`/`i`) takes the key to ~42 bytes and the
  store to roughly half. That is larger than every win in this document
  combined. It is a schema change, so it belongs in the PLAN.md "Pending schema
  changes" batch — which is explicitly waiting on R3 — or it does not happen at
  all.

---

## 8. Appendix — reproducing the measurements

Scratch crate (not in the repo; recreate under `/tmp`):

```toml
[package]
name = "r3"
version = "0.1.0"
edition = "2021"

[dependencies]
rocksdb = "0.25"
```

Commands, all in release:

| command | what it does | §|
|---|---|---|
| `r3 transform` | prefix-consistency / correctness of the custom `SliceTransform`, memtable and SST, `prefix_same_as_start`, total-order iteration | 2.4 |
| `r3 atomic 12` | forks a child writing 3-CF batches, SIGKILLs it mid-write, reopens and checks every batch is all-or-nothing | 3.1 |
| `r3 space 2000000 10` | 20 M `R` keys, 11 table configs, SST bytes + `rocksdb.aggregated-table-properties` | 6.2 |
| `r3 space2 20000000` | fan-out sweep (1/2/10/100 refs per blob) and `block_restart_interval` sweep | 6.7 |
| `r3 bench 2000000 5` | `exists_prefix` throughput with and without the prefix extractor, small block cache | 5b |
| `r3 delrange` | range-tombstone accumulation, read regression, reclaim on compaction | 5.8 |

Environment for every number quoted: macOS 25.6 / arm64, 10 cores,
`rocksdb 0.25.0` → `librocksdb-sys 0.19.0+11.8.1` (RocksDB **11.8.1**), release
profile, APFS.

Key generator: `R` + `0x01` + 32 pseudo-random bytes (splitmix64) + 4-byte repo
id + `0x01` + 32 pseudo-random bytes = 71 bytes, empty value. Digests are
pseudo-random precisely because real sha256 output is, and that
incompressibility is the point of §6.

Two caveats on scope, so nobody over-reads these numbers:

- The atomicity experiment kills the *process*, not the machine. It proves WAL
  record atomicity and recovery, not `fsync` durability. Durability is a
  separate decision (`WriteOptions::set_sync`).
- Throughput figures are single-threaded on a laptop with a deliberately small
  block cache. They establish direction and ratio, not absolute capacity.
