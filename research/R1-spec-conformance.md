# R1 — Spec and conformance

**Question:** What exactly must summ implement to pass OCI conformance, and where
are the sharp edges?

**Status:** Answered. Spec read end to end; conformance suite built and run
against the reference implementation; baseline captured.

**Sources, pinned:**

| Thing | Version |
|---|---|
| `../../distribution-spec` | `v1.1.1-57-g9727462` (main, ahead of the 1.1.1 tag) |
| `../../distribution-spec/conformance` | the **rewritten** suite: `main.go`/`run.go`/`api.go`, Go 1.24+ |
| `../../distribution` (reference impl) | `v3.1.1-63-g5b354e6f` |
| Toolchain used | Go 1.26.1 darwin/arm64 |

Everything below is written to be implemented from directly. Where the spec says
MUST vs SHOULD vs MAY, that is called out; where the *conformance suite* enforces
something the spec text only SHOULDs, that is called out too — the suite is the
gate, so its expectations are effectively MUSTs for us.

---

## 0. Headline findings

Five things that change the plan:

1. **`/v2/_catalog` is not in the spec.** It was removed before v1.0.0 (commit
   `b4e9833` "Remove _catalog API, reference as reserved"). It survives only as a
   *reserved extension namespace* in `extensions/README.md`. The conformance
   suite never calls it. It is a Docker Registry v2 legacy endpoint that
   `distribution` still serves. PLAN.md's key-schema note "`_catalog` pages over
   `n`" is still right operationally — clients and our own extension API want it —
   but it is **our** API, not a conformance obligation, and we are free to shape
   its pagination.

2. **The conformance suite has been rewritten.** The `OCI_ROOT_URL` /
   `OCI_NAMESPACE` / `OCI_TEST_PULL` / `OCI_TEST_CONTENT_DISCOVERY` variables in
   the task brief are the *legacy* ginkgo-era names. They still work — mapped in
   `conformance/config.go:confLegacyEnv` — but each prints a deprecation warning
   to stderr and several have no modern equivalent. Use the modern names
   (§1). The old "four workflows" structure is gone; the suite now reports **28
   API rows** and **24 data rows**.

3. **The reference implementation is not a passing baseline.** `distribution`
   v3.1.1 fails 91 of 852 checks at the suite's default `OCI_VERSION=1.1`
   settings. It has **no referrers API at all** (no route, no `OCI-Subject`
   header) and **no sha512 support**. Do not calibrate "correct" against
   distribution's behaviour on those two axes; calibrate against the spec.

4. **Chunked-upload `Content-Range` is not the HTTP `Content-Range`.** The spec
   requires the bare form `^[0-9]+-[0-9]+$` — e.g. `Content-Range: 0-1023` — with
   no `bytes ` prefix and no `/total` suffix. Blob *download* `Range`/`Content-Range`
   *is* ordinary RFC 9110. Two different grammars on similarly named headers.

5. **sha512 must be first-class.** PLAN.md already commits to sha256+sha512, and
   that is the right call: the suite's `OCI_DATA_SHA512=true` default runs the
   entire blob and manifest matrix a second time under sha512, and it is where
   distribution loses most of its 91 failures. Passing sha512 is ~70 checks of
   free margin over the reference implementation.

---

## 1. The harness — exact reproducible recipe

### 1.1 Build the reference registry (no Docker needed)

Docker Desktop was not running and is not required; `distribution` builds from
the local checkout and vendors everything.

```bash
# Build the reference registry
cd /Users/ren/projects/distribution
go build -mod=vendor -o /tmp/summ-conf/registry ./cmd/registry

# Config. NOTE: port 5000 on macOS is taken by AirPlay Receiver (returns 403
# with "Server: AirTunes"). Use another port.
mkdir -p /tmp/summ-conf/regdata
cat > /tmp/summ-conf/registry-config.yml <<'EOF'
version: 0.1
log:
  level: warn
storage:
  filesystem:
    rootdirectory: /tmp/summ-conf/regdata
  delete:
    enabled: true          # delete is off by default. Without it every Content
                           # Management check answers 405, which the suite
                           # downgrades from FAIL to Skip (run.go:TestFail treats
                           # errRegUnsupported as Skip) — so the run looks
                           # healthier than it is. Turn it on for a real baseline.
  tag:
    concurrencylimit: 8
http:
  addr: 127.0.0.1:15000
EOF

/tmp/summ-conf/registry serve /tmp/summ-conf/registry-config.yml &
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:15000/v2/   # -> 200
```

### 1.2 Build and run the conformance suite

```bash
cd /Users/ren/projects/distribution-spec/conformance
go build -o /tmp/summ-conf/conformance .          # README also allows: go run -buildvcs=true .

mkdir -p /tmp/summ-conf/results
cd /tmp/summ-conf && \
OCI_REGISTRY=127.0.0.1:15000 \
OCI_TLS=disabled \
OCI_REPO1=conformance/repo1 \
OCI_REPO2=conformance/repo2 \
OCI_VERSION=1.1 \
OCI_RESULTS_DIR=/tmp/summ-conf/results \
OCI_LOG=warn \
/tmp/summ-conf/conformance
```

Exit code is non-zero on any failure. Outputs land in `$OCI_RESULTS_DIR`:
`result.yaml` (machine-readable API/data matrix + redacted config), `report.html`
(full request/response transcript of every check — this is the debugging tool),
`junit.xml` (for CI).

`OCI_TLS` takes `enabled` (https, default) / `insecure` (self-signed) /
`disabled` (http). There is **no** `OCI_ROOT_URL` in the modern config —
`OCI_REGISTRY` is `host:port` only.

For a filtered debug loop:
`OCI_FILTER_TEST="OCI Conformance Test/sha256 blobs/chunked out-of-order"` —
prefix match, matches both ancestors and descendants of the given path.

`OCI_VERSION` selects the strictness profile (`conformance/config.go:214`):

| `OCI_VERSION` | Effect |
|---|---|
| `1.0` | `mountAnonymous=false`, `referrer=false` |
| `1.1` / `stable` / unset | defaults as in the README |
| `dev` / `1.1+dev` | additionally requires `Docker-Content-Digest` on blob **and** manifest responses, requires upload-cancel, and enables `?tag=` params on manifest PUT |

**Recommendation for summ:** develop against `OCI_VERSION=1.1` (the certification
profile), and add a second CI job at `OCI_VERSION=dev` as the stretch target. Everything
`dev` adds is cheap for us and is where the spec is heading.

### 1.3 Baseline against the reference implementation

Three runs, same registry instance, 13–20 s each.

**Run A — `OCI_VERSION=1.1` (the certification profile):**

```
Disabled 2 | Skip 16 | Pass 743 | FAIL 91 | Total 852   → FAIL
```

**Run B — `OCI_VERSION=dev`:**

```
Disabled 0 | Skip 25 | Pass 826 | FAIL 143 | Total 994  → FAIL
```

**Run C — distribution's honest feature set** (sha512, referrers, subjects and
non-distributable layers disabled):

```bash
OCI_REGISTRY=127.0.0.1:15000 OCI_TLS=disabled OCI_VERSION=1.1 \
OCI_DATA_SHA512=false OCI_API_REFERRER=false OCI_DATA_NONDISTRIBUTABLE=false \
OCI_DATA_SUBJECT=false OCI_DATA_SUBJECT_MISSING=false OCI_DATA_SUBJECT_LIST=false \
OCI_RESULTS_DIR=./results-min ./conformance
```

```
Disabled 2 | Skip 3 | Pass 511 | FAIL 0 | Total 516     → Pass
```

Run C is the "everything distribution actually implements works" proof. **511/516
is the floor summ must clear; 852/852 at `OCI_VERSION=1.1` is the target.**

**Result vocabulary** (`run.go:TestSkip` / `TestFail`, `state.go`):

| Status | Meaning |
|---|---|
| `Pass` | assertion held |
| `FAIL` | assertion violated |
| `Error` | the harness itself could not run the check |
| `Skip` | a prerequisite API was reported unsupported — **including a genuine failure downgraded because the registry answered with a documented "unsupported" status** (405 on delete, 202 on single-POST or mount). `errRegUnsupported` turns FAIL into Skip. |
| `Disabled` | switched off by config (`OCI_API_*` / `OCI_DATA_*`) |

The consequence: **a run with a low FAIL count is not automatically good.** Read
the API/data matrix in `result.yaml`, not just the totals — `Skip` on a row we
claim to support is a failure in disguise.

Note on counting: the console total (852) includes intermediate group nodes.
`junit.xml` emits only leaves — 690 `<testcase>` elements in Run A — while its
`tests=`/`failures=` attributes carry the 852/91 group-inclusive numbers. Both
are "correct"; do not be surprised when they disagree.

### 1.4 Where the reference implementation fails, and why

All 91 Run-A failures group into four causes:

| Cause | ~Count | Detail |
|---|---|---|
| **No sha512** | ~70 | `Docker-Content-Digest header value expected "sha512:…", received "sha256:…"`. distribution rehashes uploads as sha256 regardless of the client digest, then 400s the manifest PUT because the layer digests do not resolve. Also ignores `?digest-algorithm=sha512`. |
| **No referrers API** | 4 | `GET /v2/<name>/referrers/<digest>` → `404 page not found` with `Content-Type: text/plain; charset=utf-8` — a *router* 404, the route does not exist. Verified: no `referrers` route in `registry/api/v2/routes.go`, no `OCI-Subject` anywhere in the tree. |
| **No `OCI-Subject` header** | 11 | `manifest put failed: header value mismatch for "OCI-Subject", expected "sha256:…", received []`. |
| **Non-distributable layers** | 7 | `manifest put failed: … received 500`. distribution rejects/500s a manifest whose layers carry `application/vnd.oci.image.layer.nondistributable.*` media types with `urls` and no pushed blob. |

The **Content Management** category is clean (161/161 pass) once `delete.enabled:
true` is set, and the **`sha256 blobs`** group is 64 pass / 4 skip / 0 fail. So
the blob-upload state machine and delete semantics can be safely cross-checked
against distribution; referrers, subjects and sha512 cannot.

### 1.5 Suite structure — 28 API rows, 24 data rows

The report is a matrix: each **API row** is a capability, each **data row** is a
content shape, and the leaf tests are (data shape × API) pairs.

API rows (`conformance/state.go`, `stateAPIType`):

```
Tag listing · Tag delete · Tag delete atomic · Blob upload cancel · Blob push
Blob post only · Blob post put · Blob chunked · Blob streaming · Blob mount
Blob anonymous mount · Blob get · Blob get range · Blob head · Blob delete
Blob delete atomic · Manifest put by digest · Manifest put by tag
Manifest put with tag params · Manifest put with subject · Manifest get by digest
Manifest get by tag · Manifest head by digest · Manifest head by tag
Manifest delete · Manifest delete atomic · Referrers · Ping
```

Data rows (`OCI_DATA_*`): Image, Image Uncompressed, Image with Large Manifest,
Index, Nested Index, Empty Index, Artifact, Artifact Index, Artifact without
Layers, Artifacts with Subject, Index with Subject, Missing Subject, Custom
Fields, Data Field, No Layers, Non-distributable Layers, Bad Digest Image,
Invalid Manifest Digest, Blobs sha256, Blobs sha512, Digest Algorithm sha512,
Sparse Manifests (off by default), Tag Param (off), Tag Param sha512 (off).

Leaf test counts in Run A, largest first: `blob-delete` 106, `blob-post-put` 87,
`blob-head` 84, `blob-get` 84, `manifest-by-digest` 79, `manifest-delete` 41,
`manifest-head-by-digest` 39, `manifest-by-tag` 38, `blob-patch-chunked` 25,
`manifest-head-by-tag` 19, `tag-list` 18, `blob-post-only` 15,
`blob-patch-stream` 15, `tag-delete` 14, `referrers` 4, `blob-mount` 4,
plus singletons for the six range-request cases and the error cases.

Mapped onto the spec's four requirement categories (§Requirements):

| Category | Leaf tests (Run A) | distribution result |
|---|---|---|
| Pull | 238 | 217 pass / 21 fail |
| Push | 268 | 186 pass / 64 fail / 18 skip |
| Content Discovery | 23 | 18 pass / 5 fail |
| Content Management | 161 | 161 pass |

The spec (§Conformance) says: registries MUST support **all** of Pull; SHOULD
support Push, Content Discovery, Content Management; and if you *claim* a
category you MUST implement all of it. summ claims all four.

### 1.6 Read-only mode

For testing a preloaded registry: `OCI_API_PUSH=false` plus
`OCI_RO_DATA_TAGS` / `OCI_RO_DATA_MANIFESTS` / `OCI_RO_DATA_BLOBS` /
`OCI_RO_DATA_REFERRERS` (space-separated lists). All requests go to `OCI_REPO1`.
Useful later for benchmarking a pre-seeded 10M-repo dataset without the suite
mutating it.

---

## 2. Endpoint table

Paths are exactly as in spec §Endpoints. `<name>`, `<tag-or-digest>`, `<digest>`
grammars in §9. `<blob-push-location>` is whatever the registry returned in
`Location` — **it MUST be used verbatim**, may be absolute or relative, and may
carry query parameters that the client must preserve (spec §POST then PUT:
"The `<blob-push-location>` MAY contain critical query parameters. Additionally,
it MUST match exactly the `<blob-push-location>` obtained from the `POST`
request. It MUST NOT be assembled manually by clients").

| ID | Method | Path | Success | Failure | Required response headers on success |
|---|---|---|---|---|---|
| end-1 | `GET` | `/v2/` | `200` | `404`/`401` | — (body `{}` conventional) |
| end-2 | `GET`/`HEAD` | `/v2/<name>/blobs/<digest>` | `200` (`206` for `Range`) | `404`, `416` | `Content-Length`; `Docker-Content-Digest` (MUST — see §4) |
| end-3 | `GET`/`HEAD` | `/v2/<name>/manifests/<tag-or-digest>` | `200` | `404` (`400` on malformed digest) | `Content-Type`, `Content-Length`, `Docker-Content-Digest` (MUST) |
| end-4a | `POST` | `/v2/<name>/blobs/uploads/` | `202` | `404` | `Location`; optionally `OCI-Chunk-Min-Length` |
| end-4b | `POST` | `/v2/<name>/blobs/uploads/?digest=<digest>` | `201`/`202` | `404`/`400` | `201`: `Location`. `202` means "not supported, do POST+PUT" and needs `Location` too |
| end-4c | `POST` | `/v2/<name>/blobs/uploads/?digest-algorithm=<algorithm>` | `201`/`202` | `404`/`400` | as 4a |
| end-5 | `PATCH` | `<blob-push-location>` | `202` | `404`/`416` | `Location`, `Range: 0-<end-of-range>` |
| end-6 | `PUT` | `<blob-push-location>?digest=<digest>` | `201` | `404`/`400`/`416` | `Location` (a pullable blob URL) |
| end-7a | `PUT` | `/v2/<name>/manifests/<tag-or-digest>` | `201` | `404`/`400`/`413` | `Location`, `Docker-Content-Digest`; `OCI-Subject` if the manifest has `subject` and referrers is implemented |
| end-7b | `PUT` | `/v2/<name>/manifests/<digest>?tag=1&tag=2&tag=3` | `201` | `404`/`413`/`414`/`431` | as 7a plus `OCI-Tag` per accepted tag |
| end-8a | `GET` | `/v2/<name>/tags/list` | `200` | `404` | `Content-Type: application/json` |
| end-8b | `GET` | `/v2/<name>/tags/list?n=<int>&last=<tagname>` | `200` | `404`/`400` | as 8a; `Link` when more available |
| end-9 | `DELETE` | `/v2/<name>/manifests/<tag-or-digest>` | `202` | `404`/`400`/`405` | — |
| end-10 | `DELETE` | `/v2/<name>/blobs/<digest>` | `202` | `404`/`400`/`405` | — |
| end-11 | `POST` | `/v2/<name>/blobs/uploads/?mount=<digest>&from=<other_name>` | `201`/`202` | `404` | `201`: `Location`, `Docker-Content-Digest`. `202`: `Location` (upload session — mount refused) |
| end-12a | `GET` | `/v2/<name>/referrers/<digest>` | `200` | `400` (never `404` if implemented) | `Content-Type: application/vnd.oci.image.index.v1+json` |
| end-12b | `GET` | `/v2/<name>/referrers/<digest>?artifactType=<artifactType>` | `200` | `400` | as 12a plus `OCI-Filters-Applied: artifactType` when the filter was applied |
| end-13 | `GET` | `<blob-push-location>` | `204` | `404` | `Location`, `Range: 0-<end-of-range>` |
| end-14 | `DELETE` | `<blob-push-location>` | `204` | `404`/`400` | — |

Not in the table but implemented by every real registry and used by clients:

| — | `GET` | `/v2/_catalog?n=<int>&last=<repo>` | `200` | — | `Content-Type: application/json`; `Link` when more available |

**Redirects:** spec §API — a registry MAY answer any request with a redirect
(RFC 9110 §15.4); the documented status codes are those *after* redirects. Useful
later for S3 pre-signed blob URLs; not needed now.

---

## 3. Error codes and the JSON body

Spec §Error Codes. A `4XX` body MAY be any format. **If it is JSON it MUST be:**

```json
{
  "errors": [
    { "code": "<error identifier>", "message": "<message describing condition>", "detail": "<unstructured>" }
  ]
}
```

- `code` — MUST be present, MUST be unique, MUST contain **only uppercase
  alphabetic characters and underscores**.
- `message` — OPTIONAL; SHOULD be human readable, MAY be empty.
- `detail` — OPTIONAL, MAY be arbitrary JSON.

Note the type mismatch worth knowing about: `specs-go/v1/error.go` declares
`Detail string`, but the spec text says arbitrary JSON and `distribution` emits
objects (`"detail":{"name":"nope/nope"}`). Serialising `detail` as an object is
correct and interoperable; just be aware naive clients typed against `specs-go`
will fail to unmarshal. Prefer a string or omit it where there is no structure to
convey.

### The complete taxonomy (spec §Error Codes)

| ID | Code | Description | Typical status |
|---|---|---|---|
| code-1 | `BLOB_UNKNOWN` | blob unknown to registry | 404 |
| code-2 | `BLOB_UPLOAD_INVALID` | blob upload invalid | 400 |
| code-3 | `BLOB_UPLOAD_UNKNOWN` | blob upload unknown to registry | 404 |
| code-4 | `DIGEST_INVALID` | provided digest did not match uploaded content | 400 |
| code-5 | `MANIFEST_BLOB_UNKNOWN` | manifest references a manifest or blob unknown to registry | 400 |
| code-6 | `MANIFEST_INVALID` | manifest invalid | 400 |
| code-7 | `MANIFEST_UNKNOWN` | manifest unknown to registry | 404 |
| code-8 | `NAME_INVALID` | invalid repository name | 400 |
| code-9 | `NAME_UNKNOWN` | repository name not known to registry | 404 |
| code-10 | `SIZE_INVALID` | provided length did not match content length | 400 |
| code-11 | `UNAUTHORIZED` | authentication required | 401 |
| code-12 | `DENIED` | requested access to the resource is denied | 403 |
| code-13 | `UNSUPPORTED` | the operation is unsupported | 405 |
| code-14 | `TOOMANYREQUESTS` | too many requests | 429 |

The spec says "The `code` field MUST be one of the following". Taken literally
that closes the set — but §Legacy Docker support error codes explicitly
acknowledges clients MAY encounter others (`TAG_INVALID`, `MANIFEST_UNVERIFIED`)
and says clients SHOULD NOT depend on them. `distribution` ships four beyond the
list: `UNKNOWN` (500), `UNAVAILABLE` (503), `RANGE_INVALID` (416),
`PAGINATION_NUMBER_INVALID` (400), `TAG_INVALID` (400),
`MANIFEST_UNVERIFIED` (400).

**Decision for summ:** stay inside the 14. For the two places distribution
invents a code:

- 416 on an out-of-order chunk → use `BLOB_UPLOAD_INVALID`, not `RANGE_INVALID`.
- 400 on a bad `?n=` → use `UNSUPPORTED` or `MANIFEST_INVALID`… neither fits.
  Honestly `PAGINATION_NUMBER_INVALID` is the interoperable choice here because
  it is what every client already sees from distribution. This is the one place
  I would deviate; flag it in code with a comment. The conformance suite tests
  neither, so it is a pure interop judgement call.

Observed reference bodies, byte for byte:

```json
{"errors":[{"code":"NAME_UNKNOWN","message":"repository name not known to registry","detail":{"name":"nope/nope"}}]}
{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown","detail":"unknown tag=nosuchtag"}]}
{"errors":[{"code":"BLOB_UNKNOWN","message":"blob unknown to registry","detail":"sha256:f253c2…"}]}
{"errors":[{"code":"RANGE_INVALID","message":"invalid content range"}]}
{"errors":[{"code":"PAGINATION_NUMBER_INVALID","message":"invalid number of results requested","detail":{"n":-1}}]}
```

**Warnings** (spec §Warnings) — MAY be sent. If sent: `warn-code` MUST be `299`,
`warn-agent` MUST be `-`, `warn-date` MUST NOT be present, and total warning
bytes across all headers MUST NOT exceed 4096.
Form: `Warning: 299 - "Your auth token will expire in 30 seconds."`

**429** SHOULD carry `Retry-After` (RFC 9110 §10.2.3).

---

## 4. `Docker-Content-Digest` — when it is required

This is the header most often got wrong, so here is the exhaustive rule set.

**MUST be present** (spec text says "A successful response MUST contain the
digest … in the header `Docker-Content-Digest`"):

| Response | Spec sentence |
|---|---|
| `200` from `GET /v2/<name>/manifests/<ref>` | §Pulling manifests |
| `200` from `GET /v2/<name>/blobs/<digest>` | §Pulling blobs |
| `200` from `HEAD` on either of the above | §Checking if content exists in the registry |
| `201` from `PUT /v2/<name>/manifests/<ref>` | §Pushing Manifests — "returns the digest of the uploaded blob, and MUST be equal to the client provided digest" |

**SHOULD / conventional but not spelled as MUST:**

- `201` from `PUT <blob-push-location>?digest=…` — distribution sets it; the
  suite reads it and compares if present.
- `201` from a cross-repo mount — §Mounting a blob from another repository
  describes it ("The Docker-Content-Digest header returns the digest of the
  uploaded blob") without a MUST.

**Value rules:**

- On blob `GET`, "If present, the value of this header MUST be a digest matching
  that of the response body."
- On manifest `PUT`, it MUST equal the digest of the bytes the client sent.
- The spec permits it to *differ* from a client-supplied digest when the
  algorithms differ — that is the sha256-vs-sha512 escape hatch distribution
  relies on. **summ must not use it:** we store under the algorithm the client
  used, so we always echo the client's digest.

**What the suite enforces** (`conformance/api.go:VerifyDigest`):

```go
if digHeader == "" && flags["RequireDigestHeader"] { fail("registry did not return a Docker-Content-Digest header") }
if digHeader != "" && dig.String() != "" && digHeader != dig.String() { fail(...) }
```

So at `OCI_VERSION=1.1` (`blobs.digestHeader=false`, `manifests.digestHeader=false`)
absence is tolerated but a *wrong* value fails. At `OCI_VERSION=dev` absence
fails too. **Emit it everywhere, always, with the exact digest of the bytes.**

Suite bug worth knowing: `api.go:1003` in `ManifestPut` checks
`flags["RequestDigestHeader"]` where every producer sets `RequireDigestHeader`
— so manifest PUT never actually requires the header even in `dev` mode. Do not
rely on that; it will be fixed.

**HEAD responses** additionally MUST carry `Content-Length` = size in bytes of
the manifest/blob (§Checking if content exists). The suite asserts the exact
value *and* asserts the HEAD body is empty (`apiExpectBody([]byte{})`). A HEAD
that streams a body, or that omits `Content-Length` because the framework
suppressed it, fails.

---

## 5. Blob upload — the full state machine

Four distinct flows. All four are tested; three are effectively mandatory.

### 5.1 Flow A — POST then PUT (monolithic). §Pushing a blob monolithically

```
POST /v2/<name>/blobs/uploads/                          → 202 + Location
PUT  <blob-push-location>?digest=<digest>               → 201 + Location
     Content-Length: <length>
     Content-Type: application/octet-stream
     <whole blob>
```

`Location` on the `201` is a **pullable blob URL** — the suite immediately
`GET`s it and byte-compares (`api.go:BlobVerifyLocation`). Returning the upload
URL here instead of `/v2/<name>/blobs/<digest>` is a silent failure mode.

Reference response:

```
HTTP/1.1 201 Created
Content-Length: 0
Docker-Content-Digest: sha256:b6b6…
Location: http://127.0.0.1:15000/v2/demo/app/blobs/sha256:b6b6…
```

### 5.2 Flow B — single POST (monolithic). §Single POST, end-4b

```
POST /v2/<name>/blobs/uploads/?digest=<digest>          → 201 + Location
     Content-Length: <length>
     Content-Type: application/octet-stream
     <whole blob>
```

**OPTIONAL** — "Registries MAY support pushing blobs using a single POST
request." A registry that does not "SHOULD return a `202 Accepted` status code
and `Location` header"; the client then continues with PUT. distribution does
exactly that, which is why the `Blob post only` row reads `Skip` in the baseline
and 15 `blob-post-only` leaves are skipped.

**summ should implement it.** It is one round trip instead of two on the hot push
path, it is trivial given we already stream-hash, and it turns 15 skips into 15
passes. `Content-Length` MUST match the actual body and `<digest>` MUST match the
content (spec §Single POST).

### 5.3 Flow C — streamed PATCH (no `Content-Range`)

```
POST  /v2/<name>/blobs/uploads/                         → 202 + Location
      Content-Length: 0
PATCH <blob-push-location>                              → 202 + Location + Range
      Content-Type: application/octet-stream
      <whole blob, no Content-Range>
PUT   <blob-push-location>?digest=<digest>              → 201 + Location
      Content-Length: 0
```

This is what `docker push` / BuildKit actually do. The suite's
`BlobPatchStream` sends the PATCH with **no `Content-Range` and no
`Content-Length`** (chunked transfer encoding). A registry that requires
`Content-Range` on PATCH will fail every `blob-patch-stream` leaf (15 of them).
distribution's rule is the right one: validate the range **only if both
`Content-Range` and `Content-Length` are present**
(`registry/handlers/blobupload.go:145`).

### 5.4 Flow D — chunked PATCH. §Pushing a blob in chunks

```
POST  /v2/<name>/blobs/uploads/                         → 202 + Location [+ OCI-Chunk-Min-Length]
      Content-Length: 0
PATCH <blob-push-location>                              → 202 + Location + Range: 0-<end>
      Content-Type: application/octet-stream
      Content-Range: <start>-<end>
      Content-Length: <len>
      <chunk>
… repeat …
PUT   <blob-push-location>?digest=<digest>              → 201 + Location
      [optional final chunk, with its own Content-Range/Content-Length]
```

**`Content-Range` validation rules — all MUSTs from §Pushing a blob in chunks:**

1. Grammar is exactly `^[0-9]+-[0-9]+$`. **No `bytes ` prefix. No `/total`
   suffix.** Inclusive on both ends.
2. The first chunk's range MUST begin with `0`.
3. `Content-Length` MUST equal `end - start + 1`. Mismatch → `SIZE_INVALID`, 400.
4. Chunks MUST be uploaded in order: `start` MUST equal the previous chunk's
   `end + 1`, i.e. exactly the current committed offset.
5. **Out of order → `416 Requested Range Not Satisfiable`.** MUST. This applies
   to the closing `PUT` carrying a final chunk as well ("If the final chunk is
   uploaded out of order, the registry MUST respond with a `416`").
6. `start > end` → reject.
7. A 416 MUST NOT advance or corrupt the session. The client recovers with
   end-13 (below) and retries.

**Response to each successful chunk** MUST be `202` with:

```
Location: <blob-push-location>
Range: 0-<end-of-range>
```

`<end-of-range>` is the position of the **last uploaded byte** — so after
1024 bytes it is `Range: 0-1023`, not `0-1024`. The suite parses this by
stripping the literal `"0-"` prefix and comparing the integer
(`api.go:443`); a `Range` that does not start with `0-` fails with
"content-range header is missing the 0- prefix".

**`OCI-Chunk-Min-Length`** — OPTIONAL response header on the POST, in bytes. If
sent, every chunk except the last SHOULD be ≥ that. The suite reads it and sizes
its chunks accordingly, with a floor of `chunkMin = 1024`
(`conformance/config.go:35`) and blobs of `minChunkSize*3-5` bytes so that three
chunks are produced. If summ advertises a minimum, the suite adapts; if the
minimum is large, so are the test blobs.

**end-13 — upload status / recovery:**

```
GET <blob-push-location>   → 204 No Content
                             Location: <blob-push-location>
                             Range: 0-<end-of-range>
```

`204`, not `200`. This is the documented recovery after a 416: "A GET request may
be used to retrieve the current valid offset and upload location." The suite's
`OutOfOrderChunks` tests deliberately send a bad chunk, assert 416, then GET and
assert the offset is unchanged.

**end-14 — cancel:**

```
DELETE <blob-push-location>  → 204 No Content
       Content-Length: 0
```

SHOULD be 204. Off by default at `OCI_VERSION=1.1` (`blobs.uploadCancel=false`),
on at `dev`. Cheap for us (delete the `U <uuid>` key + the partial file), so
implement it.

### 5.5 Digest verification and the failure path

Spec §Pushing blobs: "if the client provided digest is invalid or uses an
unsupported algorithm, the registry SHOULD respond with a response code `400 Bad
Request`."

The suite's five `bad digest …` cases corrupt the blob after computing its digest
and assert:

| Flow | Expected |
|---|---|
| single POST | `400` (or `202`, treated as "single POST unsupported") |
| POST+PUT | the `PUT` returns `400` |
| chunked, close with empty PUT | the `PUT` returns `400` |
| chunked, final chunk in the PUT | the `PUT` returns `400` |
| streamed | the `PUT` returns `400` |

So: **`PATCH` never verifies the whole-blob digest** (it cannot — it has not seen
the end). Verification happens only at the `PUT`, against the `?digest=`
parameter. Use `DIGEST_INVALID`. On failure the session MUST NOT commit a blob.

The `?digest-algorithm=<algorithm>` parameter on end-4c ("SHOULD include … when
pushing a blob with a digest algorithm other than `sha256`") tells the registry
which algorithm to hash with. For summ this determines which hasher we
instantiate for the resumable-hash state in `UploadSession`. distribution ignores
it entirely, which is the root of its sha512 failures.

### 5.6 Implications for summ's design

- **`Content-Range` parser is bespoke.** Do not reach for an HTTP range crate;
  write `^(\d+)-(\d+)$`. Reject anything else with 400.
- **Two different validation regimes on the same handler.** PATCH with neither
  `Content-Range` nor `Content-Length` is a *stream* and must be accepted;
  PATCH with both is a *chunk* and must be validated. PATCH with exactly one of
  them is ambiguous — distribution treats it as a stream; do the same.
- **`start != committed_offset` → 416** is the single most-tested error path.
  The offset lives in `UploadSession` under the `U <uuid>` key. Since PLAN.md
  already stores the serialised sha2 hasher state (104 bytes) alongside it, the
  416 path is a pure read with no I/O — good.
- The 416 must leave the session byte-identical. If our chunk writer appends
  before validating, a rejected chunk corrupts the upload. **Validate the range
  against the stored offset before touching the file.**

---

## 6. Cross-repository blob mount (end-11)

Spec §Mounting a blob from another repository.

```
POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<other_name>
```

- **Success MUST be `201 Created`** with `Location: <blob-location>` — a pullable
  blob URL in `<name>`. `Docker-Content-Digest` returns the digest.
- **Refusal**: "if a registry does not support cross-repository mounting or is
  unable to mount the requested blob, it SHOULD return a `202`." The `202` is an
  ordinary upload session — `Location` is a `<blob-push-location>` and the client
  proceeds with a normal upload. Verified against the reference: mounting a
  nonexistent digest yields exactly this.
- **`from` is optional**: "The registry MAY treat the `from` parameter as
  optional, and it MAY cross-mount the blob if it can be found." The suite calls
  this *anonymous mount* and enables it by default at `OCI_VERSION=1.1`
  (`blobs.mountAnonymous: true`); it is off at `1.0`. distribution does not
  support it → 202 → the row degrades to `Skip`/unsupported rather than failing.

**summ should support anonymous mount.** With a global `L <digest>` blob record,
"is this blob present anywhere" is a single point lookup — it is nearly free, and
it is the fastest possible push path. PLAN.md's rule "A blob is servable under a
repo only if `R <digest> <repo>` is non-empty or `P <repo> <digest>` exists"
already gives the correct semantics: mounting means *inserting `P <name>
<digest>`*, nothing else. No bytes move.

Suite behaviour to be aware of (`api.go:BlobMount`): on a `202` it transparently
falls back to POST+PUT, records the API as "unsupported", and *still* verifies
the `Location` from the eventual `201` is pullable. So a wrong `Location` fails
even on the fallback path.

**Trap:** after a successful mount the suite deletes the blob from **both**
repositories (`run.go:1100`) and asserts each delete returns `202` and the blob
then HEADs `404` *in that repo*. So a per-repo delete must remove that repo's
membership edge without disturbing the other repo's — which is exactly what
`P <repo> <digest>` gives us and what a global-refcount design would get wrong.

---

## 7. Pagination and the `Link` header

### 7.1 `tags/list` (end-8a/8b) — spec §Listing Tags

- `200` with `{"name": "<name>", "tags": ["<tag1>", …]}`. `tags` MAY be empty.
- **Ordering is a MUST**: "the tags MUST be in lexical (i.e. case-insensitive
  alphanumeric order) or 'ASCIIbetical' ([Go's `sort.Strings`]) order." Those two
  descriptions are not the same thing; Go's `sort.Strings` is a plain byte-wise
  comparison, which is what the suite actually assumes. **Sort by raw bytes.**
  This is free for us — the `T <repo> <tag>` key range is already byte-ordered.
- `?n=<int>`: return at most `<int>`. "The response … MAY return fewer than
  `<int>` results, but only when the total number of tags attached to the
  repository is less than `<int>` **or a `Link` header is provided**. Otherwise
  the response MUST include `<int>` results."
- `n=0`: "this endpoint MUST return an empty list, and MUST NOT include a `Link`
  header." Verified against the reference: `{"name":"demo/app","tags":[]}` with
  no `Link`.
- `?last=<tagname>`: `<tagname>` "MUST NOT be a numerical index, but rather it
  MUST be a proper tag"; results begin **non-inclusively** after it. `n` is
  OPTIONAL when `last` is used.
- `Link` MAY be included when more tags are available; if included it MUST follow
  RFC 5988 with `rel="next"`.

**Exact `Link` format**, from `distribution/registry/handlers/catalog.go:createLinkEntry`
and confirmed on the wire:

```
Link: </v2/demo/app/tags/list?last=v1&n=1>; rel="next"
```

Path-only (relative) URL in angle brackets, query rebuilt from scratch with only
`last` and `n`, keys in alphabetical order (`last` before `n`), values
percent-encoded, fragment stripped, then `; rel="next"`.

### 7.2 `_catalog`

Not a spec endpoint (§0.1). The de-facto contract, which summ should match:

```
GET /v2/_catalog?n=<int>&last=<repo>
200  Content-Type: application/json
     Link: </v2/_catalog?last=conformance%2Frepo1&n=1>; rel="next"
     {"repositories":["conformance/repo1"]}
```

Note `/` in `last` is percent-encoded as `%2F` by `url.Values.Encode`. Repo names
must be byte-ordered — PLAN.md's "page over `n`, never `i`" rule.

Reference quirks **not** to copy:

- distribution emits `Link` even on the final page (`moreEntries` is only
  cleared when the storage driver returns `io.EOF`, which a full page never
  does). Clients then make one wasted request. summ can do better: emit `Link`
  only when a peek past the page boundary actually finds a key.
- distribution defaults `_catalog` to 100 entries and *rejects* `n` above
  `Catalog.MaxEntries` with 400. For 10M repos, prefer clamping to a maximum
  and returning `Link` — which is what its `tags/list` handler now does
  ("Per the OCI distribution-spec, a server MAY return fewer than n results when
  a Link header is provided for continuation. Clamp to MaxTags instead of
  rejecting oversized requests"). Clamp, do not reject.

### 7.3 What the suite actually tests

`run.go:TestList` — pulls the full tag list, asserts every pushed tag is present,
then (when ≥2 tags exist) sorts them, takes the midpoint as `last`, re-requests
with `?last=<midpoint>`, and asserts **everything at or before the midpoint is
absent and everything after is present**. It never sends `n`, and it never
inspects `Link`. So the `last` cursor is hard-tested; `n` and `Link` are on
honour. Implement all three correctly anyway — clients depend on them and the
suite will grow.

---

## 8. Content negotiation

`content-negotiation.md` in the spec repo is a stub ("TODO — Please see issue
#212") describing only legacy Docker schema1/schema2 behaviour. The normative
text is in §Pulling manifests:

- Client SHOULD send `Accept` listing the manifest types it supports.
- On success `Content-Type` indicates the returned type.
- The registry **SHOULD NOT** include parameters on `Content-Type`; the client
  SHOULD ignore any.
- "The `Content-Type` header SHOULD match what the client pushed as the
  manifest's `Content-Type`."
- If the manifest has a `mediaType` field, clients SHOULD reject unless it
  matches `Content-Type`.

And §Pushing Manifests: clients SHOULD set `Content-Type` to the manifest type;
the registry **SHOULD ignore parameters** on it; "If a manifest includes a
`mediaType` field, clients MUST set the `Content-Type` header to the value
specified by the `mediaType` field."

**What the suite requires** (`api.go:detectMediaType` + `ManifestGetExists`):
it parses the stored manifest JSON, takes its `mediaType` field (defaulting to
`application/vnd.oci.image.manifest.v1+json` when absent), and asserts the
response `Content-Type` contains **exactly** that string. Combined with the
byte-exact body assertion, this means:

> Store the manifest's `Content-Type` as pushed and echo it verbatim, with no
> parameters. Never synthesise it, never normalise it.

PLAN.md's `B <repo> <digest> → zstd(manifest JSON)` stores the body byte-exact;
the media type needs a home too — put it in `ManifestRecord` alongside `subject`
and `artifactType`.

Media types the suite exercises:

```
application/vnd.oci.image.manifest.v1+json          (image manifest)
application/vnd.oci.image.index.v1+json             (index, incl. nested)
application/vnd.oci.image.config.v1+json            (config blob)
application/vnd.oci.empty.v1+json                   (the empty descriptor)
application/vnd.oci.image.layer.v1.tar
application/vnd.oci.image.layer.v1.tar+gzip
application/vnd.oci.image.layer.nondistributable.v1.tar[+gzip]
application/octet-stream                            (blob upload bodies)
```

Plus arbitrary `artifactType` values on artifact manifests and indexes.

**Requests the suite sends:** every manifest GET/HEAD carries exactly two
`Accept` headers, added separately:

```
Accept: application/vnd.oci.image.index.v1+json
Accept: application/vnd.oci.image.manifest.v1+json
```

**Design decision — be permissive.** distribution 404s a stored OCI manifest when
`Accept` omits its media type:

```
$ curl -si http://127.0.0.1:15000/v2/demo/app/manifests/v1     # no Accept header
HTTP/1.1 404 Not Found
{"errors":[{"code":"MANIFEST_UNKNOWN","message":"OCI manifest found, but accept header does not support OCI manifests"}]}
```

That behaviour exists to protect schema1-only Docker clients from being handed
content they cannot parse — a problem summ does not have, since we never convert
or synthesise manifests. Recommendation: **serve the stored bytes regardless of
`Accept`**, with the stored `Content-Type`. It passes conformance (the suite
always sends a matching `Accept`), it is strictly more useful, and it removes a
whole class of "works with docker, 404s with curl" bug reports. If we ever want
strictness, gate it behind config, defaulting off.

Do handle multi-valued and comma-separated `Accept` (`a, b` in one header, and
repeated headers) if we ever do use it — RFC 7231 §5.3.2. Ignore `q=` values;
nobody sends meaningful ones.

**ETag / conditional requests** are not in the spec but distribution implements
them, and the referrers-tag-schema section explicitly says "Clients MAY use a
conditional HTTP push for registries that support ETag conditions to avoid
conflicts with other clients." Reference behaviour:

```
Etag: "sha256:9595758b361d…"       (quoted, includes the algorithm prefix)
If-None-Match: <same, quoted or unquoted>  → 304 Not Modified
```

Cheap to add and it is the only race protection available to clients using the
fallback tag schema. Worth doing for manifests.

---

## 9. Grammars — repository name, tag, digest

### Repository name (spec §Pulling manifests)

```
[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*(\/[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*)*
```

Anchored. Lowercase only. Path components separated by `/`, each component
`alphanumeric (separator alphanumeric)*` where separator is one of `.`, `_`,
`__`, or one-or-more `-`. So `foo.bar`, `foo_bar`, `foo__bar`, `foo---bar`,
`a/b/c` are valid; `Foo`, `-foo`, `foo-`, `foo..bar`, `foo/` are not.

Length: the spec gives no hard limit, only an implementers' note that many
clients cap `registry-host + "/" + <name>` at 255 characters, so registries
"should avoid values of `<name>` that would cause this limit to be exceeded".
`distribution/reference` enforces `RepositoryNameTotalLengthMax = 255` on the
path portion. **Recommendation: reject `<name>` longer than 255 bytes with
`NAME_INVALID` (400).**

An invalid name should be `NAME_INVALID` / 400. The reference instead lets the
router 404 with a plain-text `404 page not found` — permissible (any 4XX body is
allowed) but poor. summ should emit the proper JSON error.

### Tag (spec §Pulling manifests)

```
[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}
```

MUST be at most 128 characters. First character must be alphanumeric or `_`;
subsequent characters add `.` and `-`. Mixed case **is** allowed for tags
(unlike names).

`<tag-or-digest>` MUST be either a tag or a digest and "MUST NOT be in any other
format."

**Disambiguation rule** (this is a real trap): a reference containing `:` is
being offered as a digest. If it contains `:` and fails digest parsing, return
`DIGEST_INVALID` / 400 — do **not** silently treat it as a tag, which would then
404 misleadingly. This is exactly what the suite's `invalid-digest-format` test
checks by pushing to `/v2/<name>/manifests/sha256:baddigeststring`:

- `PUT` MUST return `400`.
- `GET` MUST return `400` **or** `404`.

Note `:` is not in the tag grammar anyway, so the rule is consistent.

### Digest (OCI image-spec, via `opencontainers/go-digest`)

```
[a-z0-9]+(?:[.+_-][a-z0-9]+)*:[a-zA-Z0-9=_-]+
```

Per-algorithm encoded forms, anchored:

```
sha256  ^[a-f0-9]{64}$
sha384  ^[a-f0-9]{96}$
sha512  ^[a-f0-9]{128}$
```

Lowercase hex only. summ supports sha256 + sha512 (PLAN.md); an unsupported but
well-formed algorithm should be `UNSUPPORTED` or `DIGEST_INVALID` with 400
(spec: "if the client provided digest is invalid **or uses an unsupported
algorithm**, the registry SHOULD respond with … `400`").

---

## 10. Referrers API (end-12a/12b)

Spec §Listing Referrers. Added in distribution-spec 1.1. **The reference
implementation does not have it**, so this section is sourced from the spec text
and the conformance suite only.

```
GET /v2/<name>/referrers/<digest>
GET /v2/<name>/referrers/<digest>?artifactType=<artifactType>
```

MUSTs:

- `200 OK` when the repository is found. "If the registry supports the referrers
  API, the registry **MUST NOT** return a `404 Not Found` to a referrers API
  request." — an unknown subject digest returns `200` with an **empty**
  `manifests` array, not 404.
- Invalid request (e.g. malformed digest) → `400 Bad Request`.
- `Content-Type` MUST be `application/vnd.oci.image.index.v1+json`.
- Body MUST be an image index. The suite additionally requires
  `"mediaType": "application/vnd.oci.image.index.v1+json"` **and**
  `"schemaVersion": 2` inside the body (`api.go:ReferrersList`), not just the
  header.
- Each descriptor is for a manifest **in the same `<name>`** whose `subject`
  field names `<digest>`.
- Descriptors MUST include `artifactType`, derived as:
  - image manifest: its `artifactType` if present, **else the config
    descriptor's `mediaType`**;
  - index: its `artifactType` if present, **else omit the field entirely**.
  Verified against `conformance/testdata.go:422` and `:570`, which build the
  expected descriptors with exactly this rule.
- Descriptors MUST include the annotations from the referring manifest. The
  suite checks containment, not equality (`mapContainsAll`) — extra annotations
  in the response are fine, missing ones are not.
- No matches → empty `manifests` list, still `200`.
- `Link` MUST be included when the list cannot fit in one response, RFC 5988
  `rel="next"`. No `n`/`last` parameters are defined for this endpoint, so the
  cursor lives entirely inside the URL we generate.

SHOULDs:

- "The registry SHOULD support filtering on `artifactType`." When a filter is
  requested **and applied**, the response MUST include
  `OCI-Filters-Applied: artifactType`; multiple applied filters are
  comma-separated in that one header.
- If we advertise the filter, the suite verifies the response contains **no**
  descriptor with a different `artifactType` (`run.go:2266`). Advertise only if
  the filter is exact.
- If we do *not* set the header, the suite records "registry does not set the
  expected OCI-Filters-Applied header" as *unsupported* (a soft result) and
  then requires the unfiltered set to be present. So an unfiltered response to a
  filtered query is legal — just do not claim the filter.

### `OCI-Subject` on manifest push

§Pushing Manifests with Subject: "a registry implementation that supports the
referrers API **MUST** respond with the response header
`OCI-Subject: <subject digest>` to indicate to the client that the registry
processed the request's `subject`."

The suite enforces this whenever `OCI_API_REFERRER=true` and the pushed manifest
has a `subject` (`api.go:978`). This is 11 of distribution's 91 failures.

### Subject may dangle

§Push: "A registry **MUST** initially accept an otherwise valid manifest with a
`subject` field that references a manifest that does not exist in the
repository, allowing clients to push a manifest and referrers to that manifest
in either order."

The `missing-subject` data set pushes exactly this and then queries the referrers
API for the nonexistent subject, expecting the referrer to be listed. So
`F <repo> <subject> <referrer>` must be written **without** validating that
`<subject>` resolves. Good news for us: PLAN.md's schema is a bare edge key with
no foreign-key check, so this falls out for free.

Contrast with other descriptors: "A registry **MAY** reject a manifest uploaded
to the manifest endpoint with descriptors in other fields that reference a
manifest or blob that does not exist in the registry. When a manifest is
rejected for this reason, it MUST result in one or more `MANIFEST_BLOB_UNKNOWN`
errors." So layer/config validation is optional; the suite's `OCI_DATA_SPARSE`
data set (manifests with unpushed descriptors) is **off by default**.
**Recommendation: do not validate layer existence on manifest push.** It is
optional, it costs N point lookups per push, and it makes concurrent
layer-and-manifest pushes fail. `OCI_DATA_NONDISTRIBUTABLE=true` *is* on by
default and pushes manifests referencing layers that were deliberately never
uploaded — the exact case that 500s distribution.

### The fallback tag schema

§Referrers Tag Schema. This is a **client** obligation, not a server one — but we
must understand it for two reasons.

Construction, from a subject digest:

- Truncated Algorithm = the `algorithm` section truncated to **32** characters.
- Truncated Encoded = the `encoded` section truncated to **64** characters.
- Tag = `<truncated-algorithm>` + `-` + `<truncated-encoded>`, with **any
  character not allowed by the tag grammar replaced with `-`**.

```
sha256:aaaa…(64)   → sha256-aaaa…(64)
sha512:aaaa…(128)  → sha512-aaaa…(64)          # encoded truncated to 64
test+algorithm+using+algorithm+separators+…:alsoSome=InTheEncoded…
                   → test-algorithm-using-algorithm-s-alsoSome-InTheEncodedSection…
```

That tag holds an image index with the same content the referrers API would
return. Clients that get a `404` from the referrers API MUST fall back to it, and
are responsible for maintaining it (append on push, remove on delete), with no
race protection beyond ETags.

**Why the server cares** (§Enabling the Referrers API):

1. When a registry turns the referrers API on, it **MUST** include preexisting
   image manifests that are listed in an index tagged with the referrers tag
   schema and have a valid `subject`, in the referrers API response.
2. It **MAY** include all preexisting manifests with a `subject`.
3. After enabling, it **MUST** include all newly pushed manifests with a valid
   `subject`.

**Consequence for summ:** ship the referrers API from day one and this migration
obligation never applies. If instead we shipped without it (PLAN.md defers
referrers to Phase 6), then any manifest pushed in the interim with a `subject`
must be back-filled — either by scanning `M <repo> <digest>` records for
`subject` on upgrade, or by ingesting `sha256-*` tags. **Recommendation:
populate `F <repo> <subject> <referrer>` on every manifest push starting in Phase
1, even if `/referrers/` returns 404 until Phase 6.** Writing the edge costs one
key; back-filling it later costs a full scan of every manifest in the registry.

A related question we get to decide: do we *also* honour a client-maintained
`sha256-<digest>` tag as a source of referrers? The spec's rule 1 says MUST — but
only for manifests that existed *before* the API was enabled. If summ has the API
from the start, there is no "before", and those tags are just ordinary tags.
Treat them as ordinary tags.

---

## 11. Delete semantics

Spec §Content Management. "Registries MAY implement deletion or they MAY disable
it. Similarly, a registry MAY implement tag deletion, while others MAY allow
deletion only by manifest." summ implements all three.

| Operation | Endpoint | Success | Disabled | Not found |
|---|---|---|---|---|
| Delete tag | `DELETE /v2/<name>/manifests/<tag>` | `202 Accepted` (MUST) | `400` **or** `405` (MUST be one of these) | `404` |
| Delete manifest | `DELETE /v2/<name>/manifests/<digest>` | `202 Accepted` (MUST) | `400` or `405` | `404` (MUST, if the repository does not exist) |
| Delete blob | `DELETE /v2/<name>/blobs/<digest>` | `202 Accepted` (MUST) | `400` or `405` | `404` (MUST) |

Post-conditions the spec states:

- After a tag delete, "a `GET` to `/v2/<name>/manifests/<tag>` will return a
  404" — and the manifest itself survives, reachable by digest.
- After a manifest delete, "a `GET` to `/v2/<name>/manifests/<digest>` **and any
  tag pointing to that digest** will return a 404". So deleting by digest must
  cascade to every tag pointing at it. PLAN.md's `G <repo> <digest> <tag>`
  reverse index is exactly the mechanism: scan the `G` prefix, delete each
  `T <repo> <tag>`, delete the `G` entries, delete `M` and `B`. One `WriteBatch`.

**Atomicity.** The suite has separate `… delete atomic` rows
(`OCI_API_BLOBS_ATOMIC` / `OCI_API_MANIFESTS_ATOMIC` / `OCI_API_TAGS_ATOMIC`, all
**true** by default). After each `202` it immediately issues a `HEAD` and
requires `404`. There is no retry and no grace period. So `202 Accepted`
notwithstanding, the delete **must be visible before the response is written** —
which our single-`WriteBatch` model gives us naturally, and which distribution's
eventually-consistent S3 driver historically did not (hence the config knob).

**Order.** The suite deletes tags first, then manifests in **reverse push order**
(children after parents — `run.go:1331` reverses `manOrder`), then blobs. Every
one of those must succeed independently. Do not implement "cannot delete a
manifest that is referenced by an index" — the suite deletes indexes first, but
nothing guarantees the ordering for other clients, and there is no spec basis
for refusing.

**Blob delete is per-repository.** `DELETE /v2/<name>/blobs/<digest>` removes the
blob's membership in `<name>`. Whether the bytes go away is our business (PLAN.md
defers to offline purge, and PLAN.md's rule that a blob is only servable if
`R`/`P` says so means the bytes lingering is invisible). Deleting the edge, not
the bytes, is both correct and what makes the "delete from both repos after a
mount" test in §6 pass.

**`DELETE` on a nonexistent blob returns `404`** — the suite explicitly accepts
`202`, `404`, or `405` (`api.go:BlobDelete`), treating 405 as "unsupported". But
the spec says `404` MUST be returned when the blob is not found. Return 404.

**Manifest delete accepts `400` too** in the suite's status list — `400` and
`405` are both read as "deletion is disabled".

---

## 12. Everything else that must be right

**`/v2/` (end-1).** `200 OK`. "This endpoint MAY be used for
authentication/authorization purposes, but this is out of the purview of this
specification." Reference returns `Content-Type: application/json`,
`Content-Length: 2`, body `{}`. Copy that.

**Manifest size limit.** §Pushing Manifests: a registry SHOULD enforce a maximum
manifest size and SHOULD respond `413 Payload Too Large` above it. "Client and
registry implementations SHOULD expect to be able to support manifest pushes of
**at least 4 megabytes**." The suite's `large-manifest` data set builds a
manifest with **390 annotations of 10,000 characters each** — roughly 3.92 MB,
uncomfortably close to a 4 MiB (4,194,304-byte) cap. distribution uses
`maxManifestBodySize = 4 * 1024 * 1024`. **Set summ's limit at 4 MiB minimum;
8 MiB would be safer** and costs nothing given the body is zstd-compressed into
`B <repo> <digest>`.

**Byte-exactness.** §Pushing Manifests: "The registry **MUST** store the manifest
in the exact byte representation provided by the client." The suite byte-compares
every manifest GET against what it pushed. No re-serialisation, no key
reordering, no whitespace normalisation, no BOM stripping. PLAN.md's
`B <repo> <digest> → zstd(manifest JSON)` is right; make sure the zstd round-trip
is byte-identical (it is, but test it).

**Empty blob.** `OCI_DATA_EMPTY_BLOB=true` by default. Pushing a zero-byte blob
must work end to end: `PUT` with `Content-Length: 0`, `GET` returns
`Content-Length: 0` with an empty body, `HEAD` the same, digest
`sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
The suite special-cases this (`api.go:229`: `len(val) > 0 || dig == emptyDigest`).
A `Content-Length` omitted because the body is empty will fail.

**`application/vnd.oci.empty.v1+json`** — the 2-byte blob `{}`, digest
`sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a`. Used
as the config of artifact manifests. Nothing special required, but it appears in
almost every artifact test, so a bug here is loud.

**Descriptor `data` field.** `OCI_DATA_DATA_FIELD=true` by default: descriptors
carry inline base64 `data`. Nothing to implement — just do not choke parsing it.

**Custom fields.** `OCI_DATA_CUSTOM_FIELDS=true`: manifests and configs contain
fields outside the OCI schema. **Do not validate manifests against a strict
schema**; parse the fields we need (`mediaType`, `config.mediaType`,
`artifactType`, `subject`, `layers[].digest`, `manifests[].digest`,
`annotations`) and ignore the rest. `serde` with `#[serde(default)]` and no
`deny_unknown_fields`.

**No layers.** `OCI_DATA_NO_LAYERS=true`: an image manifest with `"layers": []`.
§Pushing Manifests: "The uploaded manifest MUST reference any blobs that make up
the object. However, the list of blobs **MAY be empty**."

**Blob `Range` requests.** §Pulling blobs: "A registry SHOULD support the `Range`
request header in accordance with RFC 9110 §14." The suite tests six cases
against a 2048-byte blob and, if a case falls back to a full `200`, records
`range request unsupported, full blob returned` **as a failure** for the
`Blob get range` row. So it is a SHOULD in the spec and a MUST for the gate:

| Request | Required response |
|---|---|
| `Range: bytes=500-1499` | `206`, `Content-Length: 1000`, `Content-Range: bytes 500-1499/2048` (or `/*`), body = bytes 500..1499 |
| `Range: bytes=500-` | `206`, `Content-Length: 1548`, `Content-Range: bytes 500-2047/2048` (or `/*`) |
| `Range: bytes=-500` | `206`, `Content-Length: 500`, `Content-Range: bytes 1548-2047/2048` (or `/*`) — suffix range |
| `Range: bytes=2000-5000` | `206`, `Content-Length: 48`, `Content-Range: bytes 2000-2047/2048` (or `/*`) — end clamped to EOF |
| `Range: bytes=500-0` | `416 Requested Range Not Satisfiable` |
| `Range: bytes=5000-10000` | `416` — start beyond EOF |

Here `Content-Range` **is** the RFC 9110 form: `bytes <start>-<end>/<total|*>`.
Also send `Accept-Ranges: bytes` on the full response. Note this feeds directly
into R2 (zero-copy serving): whatever `sendfile`/`io_uring` path we choose must
support an offset+length window, not just whole files.

**Repository not found.** §Pushing Manifests: "An attempt to pull a nonexistent
repository MUST return response code `404 Not Found`." Use `NAME_UNKNOWN`.

**`?tag=` params on manifest PUT (end-7b).** OPTIONAL ("the registry MAY support
the pushing of tags specified by addition of `tag` query parameters"). If
supported: SHOULD accept ≥10 tags per request; MAY return `414`; and **MUST
include an `OCI-Tag` response header for each accepted tag**. Multiple tags may
be comma-separated within one header and/or split across repeated headers — the
suite (`run.go:2146`) accepts either. Off by default at `1.1`, on at `dev`.
Cheap for us (N extra `T`/`G` key pairs in the same batch); implement it.

**Proxying / `ns` query parameter.** §Registry Proxying. Not tested, not needed
for v1. If summ ever proxies, `ns` scopes the upstream host and the response
SHOULD echo `OCI-Namespace`. Registries MAY ignore `ns` entirely — we do.

**Legacy Docker headers.** `Docker-Distribution-Api-Version: registry/2.0` and
`Docker-Upload-UUID` are OPTIONAL (§Legacy Docker support HTTP headers: "these
headers are OPTIONAL and clients SHOULD NOT depend on them"). distribution sends
both. Sending `Docker-Distribution-Api-Version` costs nothing and placates old
tooling; `Docker-Upload-UUID` is redundant with our `Location`. Send the former,
skip the latter.

---

## 13. Sharp-edge checklist

Ordered by how expensive each is to discover late.

1. **`Content-Range` on blob PATCH is `start-end`, not `bytes start-end/total`.**
   Different grammar from blob-download `Content-Range`. §5.4.
2. **PATCH with no `Content-Range` is legal and mandatory** (the streaming flow —
   what real clients use). Validate the range only when `Content-Range` *and*
   `Content-Length` are both present. §5.3.
3. **`Range: 0-<last-byte>`, inclusive**, on every PATCH `202` and on the end-13
   `204`. Off-by-one here breaks resumable uploads silently. §5.4.
4. **end-13 is `204 No Content`, not `200`.** §5.4.
5. **Out-of-order chunk → `416`, session unchanged.** Validate before writing
   any bytes. §5.4.
6. **`Location` on a `201` is the pullable blob/manifest URL**, not the upload
   URL. The suite GETs it and byte-compares. §5.1.
7. **`Docker-Content-Digest` everywhere, always, exact.** Present on GET/HEAD of
   manifests and blobs and on manifest PUT; a wrong value fails even where a
   missing one is tolerated. §4.
8. **HEAD returns an empty body with a correct `Content-Length`.** §4.
9. **Manifest `Content-Type` is stored and echoed verbatim**, matching the
   manifest's own `mediaType`, with no parameters. §8.
10. **Manifests are stored byte-exact.** No re-serialisation. §12.
11. **A `subject` pointing at a nonexistent manifest MUST be accepted** and MUST
    appear in the referrers list. §10.
12. **`OCI-Subject: <digest>` MUST be returned** on any manifest PUT with a
    `subject`, once referrers is implemented. §10.
13. **Referrers never 404s** — unknown subject → `200` with an empty
    `manifests` array. Malformed digest → `400`. §10.
14. **`artifactType` fallback differs between manifest and index**: manifest
    falls back to `config.mediaType`; index omits it. §10.
15. **`?mount=` refusal is `202`**, and `from` is optional. §6.
16. **Delete is immediate**, not eventually consistent — the suite HEADs right
    after the `202`. §11.
17. **Deleting a manifest by digest cascades to all its tags.** §11.
18. **Blob delete is per-repository**; mounting into two repos and deleting from
    one must not affect the other. §6, §11.
19. **Tag ordering is byte-wise ascending**, and `?last=` is exclusive. §7.
20. **`n=0` returns an empty list and no `Link`.** §7.
21. **Range requests on blobs are effectively mandatory**, including suffix
    ranges and 416 on an unsatisfiable range. §12.
22. **`sha256:baddigeststring` → `400` on PUT** (not "treat as a tag"). §9.
23. **Do not schema-validate manifests**; unknown fields must round-trip. §12.
24. **Do not require referenced blobs to exist** on manifest push. §10, §12.
25. **Manifest limit ≥ 4 MiB** — the suite pushes 3.92 MB. §12.
26. **sha512 must work end to end**, including `?digest-algorithm=sha512`. §0.

---

## 14. What this means for the plan

Concrete deltas to PLAN.md:

- **`_catalog` is out of scope for conformance.** It belongs with the extension
  API in "Beyond the spec", not on the Phase 1 critical path. The `n <name>`
  key range still earns its place — `tags/list` needs the same byte-ordered
  cursor, and `_catalog` remains a headline feature.
- **Write `F <repo> <subject> <referrer>` from Phase 1**, even though
  `/referrers/` stays 404 until Phase 6. Retrofitting means a full manifest scan
  and a spec-mandated tag-schema ingest (§10). One key per push now; a migration
  later.
- **`ManifestRecord` needs `media_type` and `artifact_type`** in addition to
  `subject`. `media_type` is required for the byte-exact `Content-Type` echo
  (§8); `artifact_type` (with the config-mediaType fallback resolved **at push
  time**, so the referrers response is a pure scan) is required for the
  referrers descriptors and the `artifactType` filter (§10).
- **`UploadSession` needs the digest algorithm**, chosen at POST time from
  `?digest-algorithm=`, defaulting to sha256 — it selects which hasher's 104-byte
  state we serialise (§5.5).
- **Phase 1 exit criterion sharpens to a number**: `OCI_VERSION=1.1` with
  referrers disabled should reach the Run-C shape, ~511/516. Full 852/852 needs
  referrers, i.e. Phase 6. Add both to CI as separate jobs.
- **Blob range serving is a Phase 1 requirement, not a Phase 3 optimisation.**
  Six conformance leaves depend on it, including suffix ranges and 416. Phase 3
  makes it fast; Phase 1 must make it correct. This also constrains R2: the
  zero-copy path needs offset+length, not whole-file, semantics.
- **Work package A (conformance harness)** now has its recipe: §1. Wire the two
  runs into CI and diff `result.yaml` between runs to catch regressions in the
  API/data matrix rather than just the total.

## Appendix — artefacts from the baseline runs

Scratch location used (regenerate with §1, do not rely on it persisting):

```
/tmp/summ-conf/registry             distribution v3.1.1 binary
/tmp/summ-conf/conformance          conformance suite binary
/tmp/summ-conf/registry-config.yml  delete enabled, 127.0.0.1:15000
/tmp/summ-conf/results/             OCI_VERSION=1.1   → 743 pass / 91 fail / 852
/tmp/summ-conf/results-dev/         OCI_VERSION=dev   → 826 pass / 143 fail / 994
/tmp/summ-conf/results-min/         distribution's feature set → 511 pass / 0 fail / 516
```

`report.html` in each is the primary debugging artefact: it contains the full
redacted request and response — headers and bodies — for every single check.
When a summ run fails, open that file and read the transcript; the failure
messages alone are rarely enough.
