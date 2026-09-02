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
- **Nothing depends on redb beyond the `MetaEngine` trait.** The engine choice is
  not yet settled (see Risks in PLAN.md).
- **All mutations go through `WriteBatch`.** It is atomic, serialisable, and
  idempotent — it is the future WAL. Two rules follow: no side-channel writes
  (they would be invisible to the log and diverge replicas), and no
  non-deterministic content in a batch (no apply-time timestamps or engine-minted
  ids — the caller supplies them).
- `cargo test` before declaring anything done. `cargo clippy` and `cargo fmt`.

## Layout

```
summ-core    digest, key encoding, value types, errors
summ-meta    MetaEngine trait, WriteBatch, redb engine, repo interner
```
