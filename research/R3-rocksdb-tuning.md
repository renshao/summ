# R3 — RocksDB tuning for a four-prefix-length key schema

**Status: in progress.** Written incrementally; sections fill in as each finding
is established by experiment or by reading source.

Question: *how do we tune RocksDB for this key schema, given prefixes of four
different lengths?* The lever we most want is a **prefix bloom filter** serving
`exists_prefix()` — "is this blob still referenced?" — which purge asks once per
candidate blob, potentially billions of times.

## Outline

1. The actual prefix lengths (from `keys.rs`, not from PLAN.md's summary)
2. Option A — custom `SliceTransform`
3. Option B — column families per key type (decisive question: cross-CF batch atomicity)
4. Whole-key bloom filters and what they do for a *seek*
5. Other levers — compression, block size, `optimize_for_point_lookup`, block cache, compaction style, `DeleteRange`
6. Space — what actually reclaims the `R` keyspace
7. RECOMMENDATION

---

## 1. The actual prefix lengths

