# R5 — What the real client does, and what already exists in Rust

Answers two of the open questions in [PLAN.md](../PLAN.md): **R5** (containerd's
pull behaviour) and **R6** (survey of Rust registry implementations).

**Method.** Part A is read from source, not blogs. Reference checkouts:

| Repo | Commit / date read | Version |
|---|---|---|
| `containerd/containerd` → `../containerd` | `282a4b95`, 2026-09-02 | `2.4.0-beta` (main) |
| `containers/container-libs` (`image/`) → `../container-libs` | 2026-09-01 | `go.podman.io/image/v5` |
| `containers/image` → `../containers-image` | frozen 2025-08-29 | **moved**, see below |
| `moby/moby` → `../moby` | 2026-09-02 | main |
| `cri-o/cri-o` → `../cri-o` | 2026-09-01 | main |

Line references below are to those checkouts.

---

# Part A — what a real runtime does when it pulls

## A0. Who actually pulls

Four codebases matter, and they collapse into two:

- **containerd** — used by Kubernetes via CRI, *and* by Docker Engine. Docker's
  default image store on Linux is now containerd
  (`moby/daemon/image_store_choice.go:105` — `out := imageStoreChoiceContainerd`,
  and setting `features.containerd-snapshotter=true` now logs
  *"containerd-snapshotter is now the default and no longer needed to be set"*).
  Docker on Windows still defaults to the legacy graphdriver path.
- **containers/image** — used by podman, skopeo, buildah **and CRI-O**
  (`cri-o/go.mod` → `go.podman.io/image/v5 v5.38.0`).

  ⚠️ **The library moved.** `github.com/containers/image` has been frozen since
  2025-08-29 and its README now says *"This package was moved; please update your
  references to use `go.podman.io/image/v5` instead. New development happens on
  <https://github.com/containers/container-libs>"*. Anything you read about
  containers/image dated before late 2025 is in the wrong repo.

So: **optimise for containerd first, containers/image second.** The legacy moby
puller (`moby/daemon/internal/distribution/`) is now a Windows/opt-out path.

## A1. The exact request sequence for a containerd pull

### Resolve (always one HEAD)

`core/remotes/docker/resolver.go:245 Resolve()`:

```
HEAD /v2/<repo>/manifests/<tag>
Accept: application/vnd.docker.distribution.manifest.v2+json,
        application/vnd.docker.distribution.manifest.list.v2+json,
        application/vnd.oci.image.manifest.v1+json,
        application/vnd.oci.image.index.v1+json,
        */*
```

From the response it takes three things and nothing else:

| Header | Used for | If missing |
|---|---|---|
| `Docker-Content-Digest` | the root descriptor's digest | **falls back to a full `GET` of the manifest and hashes the body** (`resolver.go:406-459`) |
| `Content-Length` | the root descriptor's size | same fallback |
| `Content-Type` | the root descriptor's media type | media type is wrong downstream; `text/plain` is special-cased to schema1 and then rejected |

Notes with teeth:

- `405 Method Not Allowed` on a manifest HEAD makes containerd retry the *same
  path* as `GET` (`resolver.go:877-883`). A registry that does not implement HEAD
  costs every client one wasted manifest body.
- Manifests larger than `MaxManifestSize = 4 * 1048 * 1048` = **4,393,216 bytes**
  are rejected outright (`resolver.go:64`, `:461`). Note the `1048` — that is a
  typo upstream, not 4 MiB.
- When the reference is `repo@sha256:…`, containerd tries
  `manifests/<digest>` and falls back to `blobs/<digest>` on 404 only
  (`resolver.go:269-272`, and the comment at `:305` explaining why the fallback
  is deliberately narrow).
- Resolve does **not** hit `/v2/` first. There is no ping. A `401` on the real
  request drives the token dance (`resolver.go:866-876`).

### Fetch (GET only — no HEAD on the pull path)

`dockerFetcher.Fetch` (`core/remotes/docker/fetcher.go:255`) goes straight to
`GET`. It never issues a HEAD. HEAD-then-GET exists only in `FetchByDigest`
(`fetcher.go:376 createGetReq`), which is not on the pull path.

The recursion is `images.Dispatch` (`core/images/handlers.go:156`) over a handler
chain built in `client/pull.go:261` (or `core/transfer/local/pull.go:182`):
`FetchHandler → convertibleHandler → childrenHandler → distSrcLabelHandler`,
wrapped by `unpacker.Unpack`.

The **unpacker reorders the walk** (`core/unpack/unpacker.go:194-267`). For a
manifest it splits children into layers and non-layers, returns *only the
non-layers* to the dispatcher, and stashes the layers keyed by the config digest.
Layers are fetched only when the **config blob** arrives, at which point
`u.unpack` → `u.fetch` (`unpacker.go:697`) issues every layer GET at once.

So a cold multi-platform pull is:

```
1. HEAD /v2/<repo>/manifests/<tag>            ← resolve
2. GET  /v2/<repo>/manifests/<index-digest>   ← the index
3. GET  /v2/<repo>/manifests/<manifest-digest>← the platform manifest
4. GET  /v2/<repo>/blobs/<config-digest>      ← the config
5. GET  /v2/<repo>/blobs/<layer-N>            ← all layers, fired together,
                                                gated by a semaphore
```

**Steps 1-4 are strictly serial round trips.** That is four RTTs plus one small
body before the first layer byte is requested. On a 1 ms intra-DC link this is
noise; over the internet it is 200-400 ms of dead time. It also means
**manifest and config latency is on the critical path in a way layer throughput
is not** — a registry that is fast at bulk bytes but slow at small metadata reads
will lose on small images and on pod cold-start p99.

`GET /v2/<repo>/referrers/<digest>` is **not** on the pull path. `ReferrersProvider`
is nil in both `client.Pull` and the transfer service; referrers are only walked
by the OCI-archive exporter (`core/images/archive/exporter.go:149`).

### Blob request headers

`fetcher.go:505-509`:

```
Accept: <descriptor media type>, */*
Accept-Encoding: zstd;q=1.0, gzip;q=0.8, deflate;q=0.5
Range: bytes=<offset>-        ← only when resuming, or when chunking is on
User-Agent: containerd/<version>
```

## A2. Blob fetch concurrency

Two independent limits, and they share one budget.

**Limit 1 — descriptor dispatch.** `images.Dispatch` takes a limiter, but
**both call sites pass `nil`**: `client/pull.go:218,279` declares `limiter` and
never assigns it; `core/transfer/local/pull.go:236` passes `nil` literally. So the
handler fan-out is unbounded goroutines. Similarly `u.fetch` fires one goroutine
per layer immediately.

**Limit 2 — the download semaphore, which is the real one.**
`dockerBase.limiter` (`resolver.go:541,545`) is acquired in
`dockerFetcher.open` before the request and released when the body closes
(`fetcher.go:511`, `:532-537`). It is a `semaphore.Weighted` sized from
`MaxConcurrentDownloads`.

**Defaults, verified:**

| Path | Setting | Default | Source |
|---|---|---|---|
| CRI, transfer service (**the Kubernetes default**) | `plugins."io.containerd.transfer.v1.local".max_concurrent_downloads` | **3** | `plugins/transfer/plugin.go:282` |
| CRI, local pull | `plugins."io.containerd.cri".image.max_concurrent_downloads` | **3** | `internal/cri/config/config_unix.go:34` |
| Docker Engine (both stores) | `--max-concurrent-downloads` | **3** | `moby/daemon/config/config.go:32` |
| containers/image (podman, CRI-O, skopeo) | `copy.Options.MaxParallelDownloads` | **6** | `container-libs/image/copy/copy.go:40` |
| CRI-O | — | leaves it unset → **6** | `cri-o/internal/storage/image.go:872` builds `copy.Options` without it |

Configurable: yes, everywhere. But note the containerd CRI wrinkle — in 2.x the
CRI plugin defaults to the **transfer service**
(`use_local_image_pull = false`, `internal/cri/server/images/image_pull.go:187`),
and setting `max_concurrent_downloads` in the *CRI* section silently forces a
fallback to the legacy local-pull path with a warning
(`internal/cri/config/config.go:603-626`). Operators who tune the wrong knob get a
different code path, not a different number.

**Three concurrent blob GETs is the number to design for.** Not 32. The whole
premise of "spend headroom on concurrency" from `fs_limit.md` is about
*concurrent pulls from many nodes*, not concurrent layers within one pull.

## A3. Parallel chunked layer fetch (containerd 2.1+) — the big change

`fetcher.go:492-638`. Merged in
[containerd#10177](https://github.com/containerd/containerd/pull/10177)
(2025-04-24, milestone 2.1).

When `concurrent_layer_fetch_buffer` (bytes) is **> 512**, containerd splits each
blob into fixed-size chunks and fetches them with concurrent `Range` requests:

- `parallelism = MaxConcurrentDownloads` (**reused as the per-layer chunk count** —
  `fetcher.go:496`), capped at `ceil(size / chunkSize)`.
- Chunk 0 is the original request; chunks *i>0* are cloned requests with
  `Range: bytes=<offset + i*chunkSize>-` — **open-ended**, not `a-b`
  (`resolver.go:946 setOffset`).
- Each chunk worker reads exactly `chunkSize` bytes then **closes the body
  mid-response**: `io.Copy(writers[i], io.LimitReader(body, chunkSize)); body.Close()`
  (`fetcher.go:614-615`).
- Chunks are reassembled in order through a buffered `pipe` and
  `io.MultiReader`, so the digest is still computed over the whole blob in order.

**Default is off** — `ConcurrentLayerFetchBuffer` defaults to `0`
(`plugins/transfer/plugin.go:282` sets only the download/upload/unpack counts), so
`parallelism` is forced to 1 and no `Range` header is sent. But distros are
turning it on: **Bottlerocket ships `concurrent-layer-fetch-buffer = 8 MiB` by
default** and has an open incident about pull failures and faster Docker Hub
throttling as a result
([bottlerocket-os/bottlerocket#4709](https://github.com/bottlerocket-os/bottlerocket/issues/4709)).
Assume summ will see this traffic.

**Graceful degradation is built in and is the important part for us.**
`withOffsetCheck` (`resolver.go:741-770`): if the response is not `206` and
carries no matching `Content-Range`, containerd discards to the offset, logs
*"remote host ignored content range, forcing parallelism to 1"*, and continues
with a single stream. A registry that ignores `Range` is correct but slow, not
broken.

## A4. Connection reuse and HTTP/2 — **containerd is HTTP/1.1 only**

`core/remotes/docker/registry.go:254 DefaultHTTPTransport`:

```go
&http.Transport{
    Proxy: http.ProxyFromEnvironment,
    DialContext: (&net.Dialer{Timeout: 30s, KeepAlive: 30s, FallbackDelay: 300ms}).DialContext,
    MaxIdleConns:          10,
    IdleConnTimeout:       30 * time.Second,
    TLSHandshakeTimeout:   10 * time.Second,
    TLSClientConfig:       defaultTLSConfig,
    ExpectContinueTimeout: 5 * time.Second,
    ResponseHeaderTimeout: 30 * time.Second,
}
```

Read that against Go's rule: *"ForceAttemptHTTP2 controls whether HTTP/2 is
enabled when a non-zero Dial, DialTLS, or DialContext func or TLSClientConfig is
provided. By default, use of any of those fields conservatively disables HTTP/2."*
containerd sets **both** `DialContext` and `TLSClientConfig`, and never sets
`ForceAttemptHTTP2`. **containerd therefore never negotiates h2 with a registry.**
Same conclusion for containers/image: `pkg/tlsclientconfig/tlsclientconfig.go:88`
sets `DialContext`, and `docker/docker_client.go:985-986` then assigns
`tr.TLSClientConfig` — h2 disabled, `MaxIdleConns: 100`.

Consequences:

- **Keep-alive, yes; multiplexing, no.** Each concurrent blob GET is its own TCP
  (+TLS) connection.
- `MaxIdleConnsPerHost` is unset in both, so Go's `DefaultMaxIdleConnsPerHost = 2`
  applies. With 3 concurrent downloads, **only 2 connections survive into the idle
  pool**; the third is closed and re-handshaked on the next use. Turn on chunked
  fetch and the churn gets worse.
- `ResponseHeaderTimeout: 30s` — summ has 30 seconds to emit response headers
  after the request. Long metadata lookups before the first byte are fatal, not
  merely slow. (containers/image sets no such timeout.)
- CRI wraps the whole pull in `image_pull_progress_timeout`, default **5 minutes**
  of *no progress* (`internal/cri/config/config.go:63`). Progress is measured by
  bytes reaching the content store, so a stalled body kills the pull.

**Design consequence:** ALPN h2 support is optional politeness, not a
performance lever — no real client will use it. What matters is that TLS session
resumption and connection reuse are cheap, and that accepting a burst of ~3-8 new
TCP+TLS connections per pulling node is fast. Do not size the server around
"one connection per client".

## A5. Range requests and resumption

Three separate mechanisms, easy to conflate:

1. **Resume after a mid-body failure.** `httpReadSeeker`
   (`core/remotes/docker/httpreadseeker.go:47-92`): on `io.ErrUnexpectedEOF` it
   closes the body and calls `open(offset)` again, which sets
   `Range: bytes=<offset>-`. Up to `maxRetry = 3` reopens **with no forward
   progress**; any successful read resets the counter, so a flaky link can retry
   far more than 3 times overall.
2. **Resume after a restarted pull.** `content.Copy`
   (`core/content/helpers.go:190-231`) reads the existing ingest offset from the
   content store and seeks the reader to it — a pull interrupted at 900 MB of a
   1 GB layer resumes with `Range: bytes=943718400-`, not from zero.
3. **Chunked parallel fetch.** §A3.

So: **`Range` on blob GET is not optional for a production registry.** It is on
the retry path even with the default config, and podman/CRI-O additionally use
*multi*-range for `zstd:chunked` partial pulls (below).

**Legacy moby is the counter-example.** `moby/daemon/internal/distribution/xfer/download.go:269-298`
retries the *whole layer* from byte 0, up to `maxDownloadAttempts = 5`
(`moby/daemon/config/config.go:40`), with a `5s × attempt` backoff. No Range, no
resume. Another reason the containerd store is now the default.

**containers/image goes further: multi-range in one request.**
`container-libs/image/docker/docker_image_src.go:449-487 GetBlobAt` builds
`Range: bytes=a-b,c-d,e-f` and expects `206` with
`Content-Type: multipart/byteranges; boundary=…`
(`:373 handle206Response`, `:418 multipartByteRangesRe`). If the server answers
`200` it splits the full body client-side (`splitHTTP200ResponseToPartial`) —
correct but it downloads the whole blob. `400` is treated as
`BadPartialRequestError` and the caller falls back. This is the composefs /
`zstd:chunked` partial-pull path; on a node reusing an existing chunk store it can
mean pulling 5% of a layer.

## A6. Digest verification and mid-body errors

Verification is **streaming into a rolling hash, checked at commit**:
`plugins/content/local/writer.go:73` hashes on every `Write`; `:115-137` compares
at `Commit` and returns `ErrFailedPrecondition` on mismatch. Copy buffer is
**1 MiB** with `ReadAtLeast` so writes are full-buffer
(`core/content/helpers.go:39`, `:321-360`).

Two things follow for summ's error handling:

- A short body is caught before digest comparison:
  `copied < size - offset` → `io.ErrUnexpectedEOF` ("short read")
  (`helpers.go:216-219`). A truncated response is diagnosed as a network fault
  and **retried with `Range`**, not as corruption.
- Because the digest is only checked at the end, **there is no way to signal
  "this body is bad" mid-stream except by breaking the connection.** If summ
  discovers a fault after sending headers and part of the body, the correct move
  is to abort the response (drop the connection / send a broken chunked
  terminator), never to append an error document — that would silently corrupt
  into a digest mismatch that looks like registry corruption rather than a
  transient fault. Aborting gets a `Range` retry; a bad body gets a hard failure.
- `desc.Size == 0` is rejected outright with a pointed comment about
  *"a poorly configured registry/web front end which responded with no
  Content-Length header"* (`core/remotes/handlers.go:152-157`).

## A7. What the client already has — cold vs warm

The content store is content-addressed and global to the node, keyed by digest
alone — **not per repository**. `content.OpenWriter` returns
`ErrAlreadyExists` if the blob is present (`plugins/content/local/store.go:542`),
and `FetchHandler` swallows that and returns no error
(`core/remotes/handlers.go:130-132`). `childrenHandler` then reads children from
the *local* store, so the walk continues without any HTTP.

Therefore:

| Scenario | Requests to summ |
|---|---|
| Fully cold node | 1 HEAD + 2 manifest GETs + 1 config GET + N layer GETs |
| Node that shares base layers with another image | 1 HEAD + 2 + 1 + **only the missing** layer GETs |
| Same digest already fully present | **1 HEAD, zero GETs** |
| kubelet with `imagePullPolicy: IfNotPresent` and the image already in the image store | **zero requests** — CRI never calls `PullImage` |

CRI's own comment makes it explicit: *"cached layers are not counted because they
never trigger an HTTP request"* (`internal/cri/server/images/image_pull.go:241-245`).

The snapshotter is a second, deeper cache: once layers are unpacked, containerd
may drop the compressed blobs entirely (`discard_unpacked_layers`), so the node
holds the *rootfs* without the layer bytes. Re-pulling the same image then still
costs nothing because the image record exists.

**This is the single biggest realism gap in the current bench harness.** A
benchmark that drops the *server's* page cache but pulls into a *fresh* client
every time measures a case that, in a real cluster, is the minority: most pulls
share base layers and fetch a fraction of the bytes. `bench/loadtest` should grow
a "warm client" mode that skips a configurable share of layers, otherwise summ
will be tuned for a workload Kubernetes does not generate.

## A8. Registry-side behaviours that make containerd faster or slower

**Make it faster**

| Do this | Why |
|---|---|
| `Docker-Content-Digest` **and** `Content-Length` on manifest HEAD | without either, containerd does a full manifest GET it did not need (`resolver.go:406`) |
| Correct `Content-Type` on manifest HEAD *and* GET | it becomes the descriptor's media type verbatim (`resolver.go:217-230`) |
| Implement HEAD on `/manifests/` | a `405` costs a wasted GET (`resolver.go:877`) |
| `Content-Length` on blob GET | drives `remaining`, chunk arithmetic, and progress accounting; `size == 0` is a hard error |
| Honour `Range` with a real `206` + `Content-Range` | enables resume *and* chunked fetch; without it containerd degrades to one stream |
| Fast small-object reads | 4 of the 5 serial steps in a pull are ≤ a few KB. Manifest/config latency is on the critical path; layer throughput is not |
| Cheap connection setup, TLS resumption | 1 connection per concurrent blob; only 2 stay pooled |
| 307 to object storage works | redirects are followed (up to 10) and re-authorized (`resolver.go:703-713`); presigned S3 URLs are viable |

**Make it slower, or break it**

| Don't | Why |
|---|---|
| **Set `Content-Encoding` on blob responses.** | containerd advertises `zstd;q=1.0, gzip;q=0.8, deflate;q=0.5` and *will* transparently decode whatever you claim (`fetcher.go:640-664`). Layers are already compressed; re-encoding burns CPU on both ends, and a mistaken `Content-Encoding: gzip` on an already-gzipped layer makes containerd decode it and fail the digest. Serve blobs as opaque bytes, `identity`, always. |
| Rely on `429` to shed load | `retryRequest` returns `true` for `StatusTooManyRequests` and `RequestTimeout` and **retries immediately with no delay and no `Retry-After` honoured** (`resolver.go:884-886`, `:797-814`). You get 5 rapid-fire retries, then a hard failure. 429 amplifies load rather than shedding it. |
| Return 5xx for transient conditions | retried only if this is the last host *and* the previous response was a different status (`resolver.go:886-894`) — inconsistent and hard to reason about. |
| Buffer whole blobs to serve a `Range` | with chunked fetch, most chunk requests are **open-ended** `bytes=N-` and the client **aborts after `chunkSize` bytes**. A 1 GB layer at 8 MiB chunks means several requests that ask for hundreds of MB and read 8. summ must stream lazily and cancel promptly on client disconnect, and must not treat mid-body client aborts as errors worth logging per-request. |
| Take >30 s to emit response headers | `ResponseHeaderTimeout: 30s`. |
| Serve manifests > ~4.19 MB | rejected as not-found (`MaxManifestSize`). |

**Known pathologies in the wild (verified, 2026):** the containerd 2.1 chunked
fetch is the current source of registry-compatibility incidents — Artifactory
pull failures, a race condition, and *faster* Docker Hub QPS throttling because
one layer now costs N requests
([bottlerocket#4709](https://github.com/bottlerocket-os/bottlerocket/issues/4709);
the mitigation is `concurrent-download-chunk-size=0`). The PR discussion notes
S3-backed registries benefit most and Docker Hub throttles at ~60 MB/s regardless.

Also worth knowing: containerd surfaces `Warning: 299 …` response headers to the
user (`core/remotes/docker/warnings.go`), and honours an `ns=` query parameter
only when the host is configured as a proxy/mirror (`resolver.go:645-652`).

## A9. What this means for summ's server design

1. **Optimise the small-object path as hard as the byte path.** Four of the five
   serial steps in a cold pull are metadata: `HEAD manifests/<tag>`,
   `GET manifests/<index>`, `GET manifests/<manifest>`, `GET blobs/<config>`.
   They are strictly sequential, so their latencies add. This is exactly where
   the RocksDB metadata store should win over distribution's filesystem link
   walk — and it means the `M`/`B`/`T` key lookups deserve the block cache, not
   just the `R` prefix scans.

2. **Make `HEAD /manifests/<ref>` a first-class, single-lookup endpoint.**
   It must return `Docker-Content-Digest`, `Content-Length`, and `Content-Type`
   without touching the blob store. With `T <repo> <tag> → digest` and
   `M <repo> <digest> → ManifestRecord{size, media_type}` this is two point
   lookups and no body read — do **not** implement HEAD as "GET and throw the
   body away". Also implement HEAD on blobs; it is cheap and some tools use it.

3. **`Range` on blob GET is required for Phase 3, not optional.** It is on
   containerd's retry path with default config, on the chunked-fetch path that
   Bottlerocket already enables by default, and podman/CRI-O need
   *multi*-range + `multipart/byteranges` for `zstd:chunked`. Single-range first;
   multi-range is a real (if secondary) differentiator for the podman ecosystem.
   Returning `200` instead of `206` is safe everywhere — both clients degrade —
   so ship single-range early and multi-range later without fear.

4. **Design the blob path around aborted, open-ended range reads.** The chunked
   fetch pattern is: request `bytes=N-`, read 8 MiB, kill the connection. Whatever
   R2 concludes about `sendfile` vs `io_uring` vs `tokio::fs`, the test case is
   *"client cancels 8 MiB into a 900 MB response"* — near-zero wasted read-ahead,
   prompt fd release, no per-abort logging or metric cardinality.

5. **Never compress or transform blob bodies.** No `Content-Encoding`, no
   `tower-http` `CompressionLayer` anywhere near `/blobs/`. Compressing manifests
   is fine and cheap (they are JSON), but the `B` key already stores them
   zstd-compressed at rest — decompress and serve `identity`, because the digest
   is over the plaintext.

6. **Do not use `429` as a load-shedding mechanism.** containerd retries it
   immediately, five times, ignoring `Retry-After`. Since escaping provider rate
   limits is one of summ's two reasons to exist, the answer is to *not* need
   throttling: queue, or accept and stream slowly, or fail with a 5xx that at
   least does not amplify. If throttling is ever added, it needs to be a
   connection-level or accept-level control, not a status code.

7. **Size for ~3-8 concurrent connections per pulling node, HTTP/1.1, no
   multiplexing.** The concurrency that matters is *nodes × 3*, not layers.
   `maxthreads: 100`-style caps are the thing to avoid (per `fs_limit.md`), but
   equally do not over-invest in h2/h3 — no real client will use it. Do keep TLS
   session resumption on and connection setup cheap, because containerd only
   pools 2 idle connections per host and re-handshakes the rest.

8. **Emit headers early.** 30 s `ResponseHeaderTimeout`, 5 min CRI no-progress
   timeout. Resolve metadata, then start streaming; never block on a slow path
   before the status line.

9. **Abort, do not apologise.** If a blob body fails mid-stream, tear the
   connection down. containerd will diagnose a short read and re-request with
   `Range`. Appending anything to the body converts a retryable fault into a
   digest mismatch.

10. **Fix the benchmark before trusting it.** `bench/loadtest` currently models
    `GET manifest → GET all blobs concurrently` at `--blob-concurrency 3`
    (per `../container-registry/CLAUDE.md`). That is close on concurrency but
    wrong on shape. To be honest it needs: the leading `HEAD /manifests/<tag>`;
    the index→manifest→config→layers serialisation; and a warm-cache mode that
    skips already-held layers. Otherwise summ gets tuned for a pure-throughput
    workload while real pulls are dominated by four serial metadata round trips
    and a partial layer set.

---

# Part B — the Rust ecosystem

Everything below was verified against crates.io and the GitHub API on
**2026-09-02**. "Last activity" is the repository `pushed_at`, which includes
branch pushes, so it is an upper bound on liveness — I have noted where the
default-branch commit is materially older.

## B1. Full registry implementations in Rust

### Trow — <https://github.com/Trow-Registry/trow>
★1029 · last activity **2026-09-01** · Apache-2.0 · v0.10.0

The one to read. A registry aimed at Kubernetes clusters, with an admission
controller built in. Its stack is startlingly close to what summ has already
chosen:

| Concern | Trow's choice |
|---|---|
| HTTP | `axum` 0.8 + `axum-server` (rustls), `tower-http` |
| Blob storage | plain filesystem, `src/file_storage.rs` |
| Metadata | **SQLite via `sqlx` 0.8**, three migrations in `migrations/` |
| Types | `oci-spec` 0.9, `oci-client` 0.16 (for its pull-through proxy) |
| Hashing | `sha2` 0.11 |

**What is worth stealing:**

- **The multi-level repository-name problem, solved.** OCI repo names contain
  `/`, so `/v2/{name}/blobs/{digest}` is not expressible in axum's router. Trow
  generates seven route variants (`/v2/{one}/…` … `/v2/{one}/…/{seven}/…`) with a
  `route_7_levels!` / `endpoint_fn_7_levels!` macro pair
  (`src/routes/macros.rs`), and reassembles the name in the handler. Ugly, and
  correct, and Package B will hit exactly this on day one. The alternative is a
  fallback route plus manual suffix parsing; Trow's macro is the cheaper first
  cut and caps names at 7 segments.
- **Its schema is a relational restatement of summ's key schema** —
  `blob(digest, size)`, `manifest(digest, json, blob)`,
  `repo_blob_assoc(repo_name, blob_digest, manifest_digest)`,
  `manifest_blob_assoc(...)`, `tag(repo, tag, manifest_digest)`, plus an
  `AFTER INSERT` trigger that extracts layer digests from the manifest JSON with
  `json_each(json_extract(NEW.json, '$.layers'))`. One row per edge, exactly the
  fan-in rule from PLAN.md. Independent convergence on the same design is a
  useful sanity check.
- **One real divergence to note:** Trow keys manifests by **digest alone**
  (`manifest.digest` is the PK, global), with repo membership expressed only
  through `repo_blob_assoc`. summ keys them per repo (`M <repo> <digest>`).
  Trow's shape deduplicates identical manifests across repos; summ's shape makes
  "is this manifest servable under this repo?" a single seek instead of a join.
  Given PLAN.md's explicit warning about leaking content across repos, summ's
  choice is the safer one — but it is worth being conscious that it is a choice.

**What it demonstrates does not scale, which is summ's whole thesis:**

```sql
SELECT DISTINCT rba.repo_name FROM repo_blob_assoc rba
WHERE rba.repo_name > $1 ORDER BY rba.repo_name ASC LIMIT $2
```
(`src/repositories/repo_blob_assoc_repository.rs:171`)

`_catalog` is a `DISTINCT` over the *edge* table. At 10M repos × ~20 blobs that is
a scan over 2×10⁸ rows to return one page. summ's `n <name> → repo id` interner
keyspace answers the same question with a prefix seek of exactly `limit` keys.
This single query is the clearest possible justification for the metadata design
in PLAN.md — cite it.

Also: Trow does **not** implement `Range` on blob GET (`src/routes/blob.rs` is
`get` only, `src/routes/response/blob_reader.rs` sets `Content-Length` and
streams). Per Part A that is survivable but leaves throughput on the table.

### Angos — <https://github.com/project-angos/angos>
★57 · last activity **2026-08-31** · Apache-2.0 · v1.6.1

The most ambitious active Rust registry, and architecturally the *opposite* of
summ. Workspace of `angos` + `angos-oci` + `angos-storage` (fs + S3) +
`angos-s3-client` + `angos-backoff` + `angos-conformance-gates`. Raw **hyper**,
not axum. Redis for caching and distributed locking. Feature list includes
online GC, pull-through cache, immutable tags, CEL access policies, mTLS, OIDC.

**Its metadata store is the object store.** `src/registry/metadata_store/` builds
link keys (`src/registry/keys.rs`: `blob_ref_path`, `blob_ref_own_path`,
catalog index keys) directly on `ObjectStore`, i.e. distribution's link-file
model, rebuilt more carefully — reference keys live *outside* `v2/blobs/`, and
namespaces are terminated with `!` so `a`'s leaves cannot collide with `a/b`'s
directories. PLAN.md rejects this model explicitly. Angos is the strongest
existing argument *for* it: no database means no second source of truth to
diverge and no HA story to invent. It is also why angos needs Redis.

**Two things worth reading even though the model is rejected:**

- **Online GC with grace periods** (`metadata_store/gc.rs`,
  `DEFAULT_GC_GRACE_SECS = 300`, "a collector's range marker outlives its last
  refresh"). PLAN.md's Risk 4 says offline purge is a scaling cliff whose upgrade
  path is upload-session pinning plus an mtime grace period. Angos has shipped
  that shape. Read it before designing Phase 4's successor.
- **`crates/conformance-gates` + `conformance/`** — a worked example of wiring the
  OCI conformance suite into CI, which is Package A.

### Nora — <https://github.com/getnora-io/nora>
★293 · last activity **2026-08-30** · v1.2.2 · `nora-registry` on crates.io

A multi-format artifact registry (Docker/OCI + 14 other protocols) in axum.
Explicitly *"Filesystem is the database … In-memory indexes are rebuilt on
startup"* (`ARCHITECTURE.md`). That is a hard ceiling at summ's target: a startup
scan and a resident index over 10M repos is not viable. Useful as a
counter-example and for its `COMPAT.md`, which is a decent per-endpoint
conformance checklist. Not a source of reusable code.

### Others, verified but not recommended reading

| Project | State |
|---|---|
| [mcronce/oci-registry](https://github.com/mcronce/oci-registry) ★196 | Pull-through caching registry with S3 backing. **Last activity 2024-06-24** — dormant ~2 years. |
| [mbr/container_registry-rs](https://github.com/mbr/container_registry-rs) ★15, crate `container-registry` 0.3.1 | "Minimal OCI registry, usable as crate or binary". **Last activity 2024-08-14.** 32k downloads. Dormant. |
| [PThorpe92/Floundr](https://github.com/PThorpe92/Floundr) ★20 | Axum/Tokio registry + Ratatui TUI. Last activity 2024-09-10. Abandoned. |
| [gpmcp/registry-testkit](https://github.com/gpmcp/registry-testkit) ★3 | Deliberately minimal test registry. Last activity 2025-12-11. |
| `ferro-oci-server` 1.0.0, `holger-*-repository`, `sui-registry`, `distribution` 0.0.1 (arcboxlabs), `oci-zero`, `zlayer-registry` | All published 2026, all with **0-3 GitHub stars and 43-1600 total downloads**. Several are clearly machine-generated crate families (`engenho-*`, `holger-*`, `use-oci-*` publish dozens of sibling crates on the same day). Verified to exist; none has meaningful review, adoption, or a maintainer track record. **Do not depend on any of these.** |

**Conclusion for Part B, registries:** there is no Rust registry that summ should
fork or build on. Trow is the closest and most useful to *read*; angos is the
best counter-argument to read; nothing is a dependency.

## B2. Building blocks worth considering

### `oci-spec` 0.10.0 — the one clear win
crates.io: 18.4M downloads · repo <https://github.com/youki-dev/oci-spec-rs>
★296 · last activity **2026-08-27** · Apache-2.0

⚠️ **The repo moved** from `containers/oci-spec-rs` to `youki-dev/oci-spec-rs`.
The crate is unchanged; only the home is different.

Pure serde types with **no HTTP stack and no async runtime** —
`const_format`, `serde`, `serde_json`, `thiserror`, `derive_builder`, `getset`,
`strum`, `regex`. Feature-gated into `image` / `runtime` / `distribution`; summ
would take `image` + `distribution` and drop `runtime`.

What it gives summ directly:

| Module | Type | Use in summ |
|---|---|---|
| `distribution::ErrorCode` | 14-variant enum, `SCREAMING_SNAKE_CASE` serde, incl. the `TOOMANYREQUESTS` special case | **the spec error taxonomy, done** — R1 calls this a sharp edge |
| `distribution::{ErrorResponse, ErrorInfo}` | the `{"errors":[…]}` body | error responses |
| `distribution::{RepositoryList, TagList}` | `_catalog` and `tags/list` bodies | pagination responses |
| `image::{ImageManifest, ImageIndex, Descriptor, MediaType}` | manifest/index parsing | extracting layer + child digests for the `R`/`S`/`F` keys |
| `image::Digest`, `DigestAlgorithm`, `Sha256Digest` | validated parse: requires `alg:value`, checks per-algorithm hex length, **rejects uppercase hex** | the HTTP-boundary digest parser |

Two caveats, both easy to live with:

- `image::Digest` is a **string** wrapper (`{ algorithm, value: Box<str>, split }`).
  summ's `summ-core::Digest` is a tagged enum over raw bytes because the key
  encoding needs an algorithm byte + raw hash. These are complementary, not
  competing: parse and validate with `oci_spec::image::Digest` at the edge,
  convert once into summ's binary form. Do **not** replace `summ-core`'s digest.
- **Never re-serialise a manifest.** `ImageManifest` round-trips through serde,
  which will not reproduce the pushed bytes, which would break the digest.
  PLAN.md already stores the body byte-exact under `B <repo> <digest>` — the rule
  is: `oci-spec` is for *reading* structure out of a manifest, and the raw bytes
  are what is stored and served. Worth stating in CLAUDE.md.

Trow already depends on `oci-spec` for exactly these purposes, which is
independent evidence it fits a server and not just a client.

### `oci-client` 0.17.0 — for tests and the bench harness only
crates.io: 6.9M downloads · repo <https://github.com/oras-project/rust-oci-client>
★185 · last activity **2026-08-26** · Apache-2.0 · ORAS project (CNCF)

The successor to krustlet's `oci-distribution` (whose repo,
`krustlet/oci-distribution`, now **404s**; the crate's last release was
2024-03-27 — it is dead, use `oci-client`). Depends on `oci-spec` 0.10,
`reqwest` 0.13, `sha2` 0.11 — the same versions summ is on.

It has `max_concurrent_download` / `max_concurrent_upload` config
(`src/client.rs:2388-2398`), `buffer_unordered` fan-out, `RANGE` header support
and `StatusCode::PARTIAL_CONTENT` handling (`src/client.rs:1422-1483`). That
makes it a credible **client** for integration tests and for a more realistic
`bench/loadtest` — but per §A9.10, the harness should model containerd's actual
sequence, and `oci-client` will not do that for you out of the box (it is a
library-shaped puller, not a containerd emulation). Useful as a second opinion
and as a push client in tests. **Not a server dependency.**

### The rest, verified

| Crate | Version / activity | Verdict |
|---|---|---|
| `docker_credential` 1.4.0 · <https://github.com/keirlawson/docker_credential> · 35M downloads · 2026-05-19 | healthy | Client-side credential-helper reading. Irrelevant to a server; possibly useful in the bench harness. |
| `docker-registry` 0.9.0 · <https://github.com/clowdhaus/docker-registry> · ★5 · 2026-03-07 | a maintained-ish fork of dkregistry | Client. Tiny audience. No reason to prefer it to `oci-client`. |
| `dkregistry` 0.5.0 · <https://github.com/camallo/dkregistry-rs> · ★73 | **last release 2020-10-07**, 35 open issues | Dead. This is the "beautiful unmaintained crate" case. Avoid. |
| `oci-registry-client` 0.2.3 · <https://github.com/ecarrara/oci-registry-client> ★18 · 2026-01-19 | tiny | Client. No. |
| `ocipkg` 0.4.0 · <https://github.com/termoshtt/ocipkg> ★69 · 842k downloads · 2026-02-16 | active-ish | Ships static libraries as OCI artifacts. Wrong problem. |
| `oci-wasm` 0.6.0 · bytecodealliance · 2026-07-16 | healthy | Wasm-specific wrapper over `oci-client`. Not relevant. |
| `oci-tar-builder` 0.4.0 · containerd/runwasi | **last published 2024-03-20** | OCI tar archives. Not relevant. |
| `sigstore` 0.14.0 · sigstore-rs · 2026-05-22 | active, self-described "experimental" | Only if summ ever verifies signatures. Not now. |
| `http-range-header` 0.4.2 · 113M downloads · **2024-11-28** | no-dep, feature-complete, stable-by-completion | Range parsing. Low risk despite the date — it does one thing and `tower-http` depends on it. |
| `headers` 0.4.1 · hyperium · 151M downloads · 2025-06-02 | healthy | Typed `Range`, `Content-Range`, `ETag`. Same ecosystem as hyper/axum. |
| `tower-http` 0.7.1 · 429M downloads · 2026-08-31 | healthy | Already implied by axum. Use it for tracing/CORS/set-header — **not** `CompressionLayer` on `/blobs/` (see §A9.5). Its `ServeFile` does Range but assumes a filesystem path and its own headers; summ's blob handler needs its own. |
| `axum-range` 1.0.0 · ★small · 2025-09-16 | single-maintainer, 110k downloads | Tempting shortcut for Range responses. Small enough that writing the ~150 lines is cheaper than the dependency. |

### Explicitly checked and **not found**

- No Rust crate implements the OCI **distribution-spec server** in a form worth
  depending on. The conformance suite itself is Go
  (`../../distribution-spec/conformance`) and will have to be driven as an external
  binary from CI — angos's `conformance-gates` shows one way.
- No Rust equivalent of `go-containerregistry`/`crane` with server-side utility.
- No maintained Rust crate for repository-**name** validation against the spec
  grammar. `oci_spec::distribution::Reference` parses *client-side* refs
  (`registry/repository:tag@digest`), not the server's `<name>` path component.
  summ writes that regex itself; it is ten lines.

## B3. Dependency recommendation

| Crate | Purpose in summ | Maintained? | Use it? | Why |
|---|---|---|---|---|
| **`oci-spec`** (features `image`, `distribution`) | Wire types: `ErrorCode`/`ErrorResponse`, `ImageManifest`/`ImageIndex`/`Descriptor`, `RepositoryList`/`TagList`, `Digest` parsing/validation | **Yes** — 18.4M dl, ★296, active 2026-08-27, `youki-dev/oci-spec-rs`, Apache-2.0 | **Yes — the one clear adopt** | Removes the error-taxonomy and manifest-parsing work R1 flags as sharp edges. Pure serde, no HTTP/runtime deps, feature-gated. Trow uses it in a server. Risk if abandoned is low: the types are the spec, and vendoring them is a day's work. |
| `sha2` 0.11 + `digest`/`crypto-common` 0.2 | sha256/sha512, and `hazmat::SerializableState` for resumable upload hashing | **Yes** — 0.11.0 stable since 2026-03-25, RustCrypto | **Yes (already decided)** | PLAN.md's resumable-hashing design depends on it. `oci-client` and Trow are both on 0.11, so the ecosystem has moved. |
| `axum` 0.8 + `tower-http` 0.7 | HTTP layer (Package B) | **Yes** — 450M dl, active 2026-04-14 | **Yes** | Trow is proof it carries a real registry. Budget for the multi-segment repo-name routing problem (§B1) on day one. **Do not** enable `CompressionLayer` on `/blobs/`. |
| `headers` 0.4 and/or `http-range-header` 0.4 | Parse `Range`, emit `Content-Range` (Phase 3) | **Yes** (`headers` 2025-06; `http-range-header` 2024-11 but complete and depended on by `tower-http`) | **Yes, thin** | Range parsing has nasty edges (suffix ranges, multi-range, unsatisfiable). Do not hand-roll the parser; do hand-roll the response. |
| `oci-client` 0.17 | Pull/push client for integration tests and a more honest `bench/loadtest` | **Yes** — ORAS/CNCF, ★185, active 2026-08-26 | **Yes, dev/bench only** | Never in `summ-server`. Note it will not reproduce containerd's request sequence — for that the harness needs the shape in §A1 written explicitly. |
| `docker_credential` 1.4 | Reading `~/.docker/config.json` in bench tooling | Yes — 35M dl | Only if the harness needs registry auth | Trivial, well-used, but not on any server path. |
| `axum-range` 1.0 | Range responses | Marginal — 110k dl, small single-maintainer project | **No** | Too small a saving for a dependency on the hot path. Write it. |
| `container-registry` 0.3 (mbr) | A registry as a library | **No** — last activity 2024-08-14 | **No** | Dormant, minimal, wrong scale. |
| `dkregistry` 0.5 | Registry client | **No** — last release 2020, 35 open issues | **No** | Textbook liability. |
| `oci-distribution` 0.11 (krustlet) | Registry client | **No** — repo 404s, last release 2024-03-27 | **No** | Superseded by `oci-client`. |
| `ferro-oci-server`, `holger-*-repository`, `sui-registry`, `distribution` (arcboxlabs), `oci-zero`, `zlayer-registry` | Claim to be server-side OCI primitives | **No meaningful signal** — 0-3 stars, 43-1.6k downloads, several from bulk-publishing crate families | **No** | Existence verified; adoption, review and maintainer track record all absent. Depending on any of these is strictly worse than writing the code. |

**Net effect on PLAN.md:** one adopt (`oci-spec`), one dev-dependency
(`oci-client`), and confirmation that the axum + filesystem + embedded-KV shape is
what a real Rust registry looks like. R6 is closed; nothing in the ecosystem
changes the architecture, and nothing in it is worth forking.

**Two follow-ups this research surfaced that are not R5/R6:**

1. `bench/loadtest` does not model containerd's request sequence (§A9.10). This
   affects Phase 0's baseline and Phase 3's A/B — the numbers will be optimistic
   about metadata latency and pessimistic about cache hits.
2. Trow's `_catalog` query (§B1) is the empirical version of PLAN.md's argument
   and should be captured in Package G's benchmark as the "how everyone else does
   it" comparison point.

---

## Sources

Code read locally (see the checkout table at the top). External references:

- [containerd#10177 — Multipart layer fetch](https://github.com/containerd/containerd/pull/10177) (merged 2025-04-24, milestone 2.1)
- [containerd#9922 — Parallelise layer downloads](https://github.com/containerd/containerd/issues/9922)
- [bottlerocket-os/bottlerocket#4709 — image pull issues on aws-k8s-1.33/1.34](https://github.com/bottlerocket-os/bottlerocket/issues/4709)
- [containers/image README — migration to `go.podman.io/image/v5` / containers/container-libs](https://github.com/containers/image)
- [Podman blog — upcoming migration of three containers repositories to a monorepo](https://blog.podman.io/2025/08/upcoming-migration-of-three-containers-repositories-to-monorepo/)
- [Trow-Registry/trow](https://github.com/Trow-Registry/trow)
- [project-angos/angos](https://github.com/project-angos/angos)
- [getnora-io/nora](https://github.com/getnora-io/nora)
- [youki-dev/oci-spec-rs](https://github.com/youki-dev/oci-spec-rs)
- [oras-project/rust-oci-client](https://github.com/oras-project/rust-oci-client)
- [mcronce/oci-registry](https://github.com/mcronce/oci-registry)
- [mbr/container_registry-rs](https://github.com/mbr/container_registry-rs)
- [camallo/dkregistry-rs](https://github.com/camallo/dkregistry-rs)
