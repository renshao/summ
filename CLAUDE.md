# summ

An OCI Distribution Spec compliant container registry in Rust.

@PLAN.md

## Working rules

- **PLAN.md is the source of truth** for scope, decisions, schema, and status.
  Update it as work lands rather than restating it elsewhere.
- **No stored value may grow with the size of the registry.** Fan-in goes in its
  own key range, one key per edge. If something wants a read-modify-write, the
  schema is wrong.
- **No unbounded list API.** Every scan takes a cursor and a limit. The target is
  10M repos and up to 10M manifests in one repo.
- **RocksDB is the v1 engine**, statically linked. Nothing may depend on it
  beyond the `MetaEngine` trait; redb is kept as a second implementation and the
  integration suite runs against both, which is what keeps that boundary honest.
- **Blob storage holds bytes, not relationships.** Content-addressed
  `digest -> bytes` only. Do not reproduce distribution's link files — RocksDB
  owns every relationship, and a second source of truth will diverge.
- **Blobs land and fsync before the metadata batch commits.** An orphan blob is
  garbage; metadata pointing at a missing blob is corruption.
- **All mutations go through `WriteBatch`.** It is atomic, serialisable, and
  idempotent — it is the future WAL. Two rules follow: no side-channel writes
  (they would be invisible to the log and diverge replicas), and no
  non-deterministic content in a batch (no apply-time timestamps or engine-minted
  ids — the caller supplies them).
- `cargo test` before declaring anything done. `cargo clippy` and `cargo fmt`.

## Layout

```
summ-core    digest, key encoding, value types, errors
summ-meta    MetaEngine trait, WriteBatch, RocksDB + redb engines, repo interner
```
