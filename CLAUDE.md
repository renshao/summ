# summ

An OCI Distribution Spec compliant container registry in Rust. One binary, no
dependencies, a built-in web UI, and metadata discovery as a first-class
operation rather than an afterthought.

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
- **No `skip_serializing_if` on a stored record.** postcard is not
  self-describing, so a skipped field is not "absent" on the wire — it is
  missing, and the decoder reads the next field's bytes. `PLAN.md` records the
  bug this caused; `summ-core/tests/postcard_roundtrip.rs` is the guard.
- **All mutations go through `WriteBatch`.** It is atomic, serialisable, and
  idempotent — it is the future WAL. Two rules follow: no side-channel writes
  (they would be invisible to the log and diverge replicas), and no
  non-deterministic content in a batch (no apply-time timestamps or engine-minted
  ids — the caller supplies them).
- **Discovery is a headline feature, not an extra.** The extension API and the
  embedded UI are core product surface. They are unstandardised, so they carry
  their own tests — the conformance suite will not cover them.
- **The binary stays self-contained.** No external database, object store or
  sidecar required to run. A feature needing one must justify itself against
  that, and the usual answer is to build it in.
- `cargo test` before declaring anything done. `cargo clippy` and `cargo fmt`.

## Layout

```
summ-core     digest, key encoding, value types, errors
summ-meta     MetaEngine trait, WriteBatch, RocksDB + redb engines, repo
              interner, schema version + migration seam
summ-storage  filesystem blob store (Unix only: pread/pwrite)
summ-registry ops layer — spec operations as WriteBatch builders
summ-server   axum HTTP layer, /v2/ route table, the `summ` binary, and
              `backend.rs` — the one module that wires the three above together
```

`summ-server` reaches everything below it through `seam::Registry`, whose
failures are in spec vocabulary. **Nothing under `handlers/` may import
`summ-registry`, `summ-meta` or `summ-storage`** — `backend.rs` is the only
module that names them, and `memory.rs` is a second implementation of the same
trait, kept so the seam stays one.
