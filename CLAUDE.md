# summ

An OCI Distribution Spec compliant container registry in Rust. One binary, no
dependencies, a built-in web UI, and metadata discovery as a first-class
operation rather than an afterthought.

@PLAN.md

## Working rules

- **Commit on main branch** unless I speficially ask you to branch out
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
summ-server   axum HTTP layer, /v2/ route table, /api/v1/ discovery API, the
              embedded web UI (`ui/` + `src/ui.rs`), the `summ` binary, and
              `backend.rs` — the one module that wires the three above together
```

`summ-server` reaches everything below it through `seam::Registry`, whose
failures are in spec vocabulary. **Nothing under `handlers/` may import
`summ-registry`, `summ-meta` or `summ-storage`** — `backend.rs` is the only
module that names them, and `memory.rs` is a second implementation of the same
trait, kept so the seam stays one.

## The discovery API and the UI

- **`/api/v1/`'s route table is flat, and must stay flat.** Each collection is
  its own top-level resource (`repositories`, `tags`, `manifests`) with the
  repository name running to the end of the path. Nesting a collection under the
  name is ambiguous when a registry holds both `foo` and `foo/tags`, and the
  wrong resolution does not 404 — it answers with the other repository's data.
- **Every count is bounded and carries `complete`.** There is no stored total
  and there must not be one; a count folds pages to `seam::COUNT_CEILING` and
  reports whether it reached the end. A number that is silently wrong above a
  threshold is worse than no number.
- **Discovery reads go through `spawn_blocking`.** They are the one read path
  that is a fold rather than a point lookup.
- **The UI ships in the binary and loads nothing from the network.** No build
  step, no framework, no CDN — a registry runs air-gapped. Nothing reaches the
  DOM as a string either: repository names, tags and annotations are all pushed
  by whoever can reach the registry.
- **The product is `Summ`; the binary, the crates and the paths are `summ`.**
  Prose and anything a person reads — the UI title, the topbar, the README —
  take the capital. Identifiers do not: `summ serve`, `summ-server`, `SUMM_LOG`
  and the startup banner are the executable's name, not the product's.
- **The logo has one definition, `summ-server/ui/logo.svg`** — a sigma, for
  summation, served at `/logo.svg` as the favicon. The topbar carries an inline
  copy of the path so the tile can take `--accent` and follow the theme, which
  an `<img>` cannot; the two are the only duplication and must stay in step.
- **The discovery API and the UI carry their own tests** (`tests/discovery.rs`),
  because no conformance suite covers them and none ever will.
