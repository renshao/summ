# Schema changes from `research/R4` — the applied batch

The record of one completed change, kept out of PLAN.md because it is history:
every item below is **already in the schema**. Batched deliberately so the key
schema moved once rather than three times, and applied together once R1 (spec)
and R3 (RocksDB tuning) had landed; the prefix extractor moved to
`summ.prefix.v2` in the same pass, exactly once.

Useful when you want to know *why* a field exists. What is still open from this
batch — the deferred items and the things found during implementation — stayed
in PLAN.md.

The original list follows.

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
- **Add the analytics ranges `A`, `H` and `J`** in the same pass, and bump the
  prefix extractor to `summ.prefix.v2` once. Full rationale under **Analytics**;
  the point of batching them here is that the extractor name should move exactly
  once.
- **`MetaEngine` gains `scan_keys`** (or `Page` becomes value-optional) — purge
  scans millions of valueless edge keys and currently allocates an empty `Vec`
  per row.
