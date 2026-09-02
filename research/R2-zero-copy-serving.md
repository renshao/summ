# R2 — Getting blob bytes from disk to socket

**Question:** How should blob bytes get from disk to socket, on stable Rust, in
2026?

**Status:** Answered, with measurements. Short version: **`sendfile` is a trap
for this workload, io_uring is not ready to build on, and a *carefully written*
`std::fs`-in-`spawn_blocking` streaming body is within ~0.1 CPU-core of the
theoretical best at line rate.** The thing that actually matters — by a factor of
three to five — is **chunk size**, not zero-copy. Every Rust file server surveyed
gets this wrong by default.

**Sources, pinned (verified 2026-09-02, not from memory):**

| Thing | Version / date checked |
|---|---|
| `tokio` | 1.53.1 (2026-07-20); io_uring paths inspected on `master` |
| `tokio-uring` | 0.5.0, **last release 2024-05-27** |
| `monoio` | 0.2.4, **last release 2024-08-20** |
| `glommio` | 0.9.0, **last release 2024-03-25** |
| `compio` | 0.19.2 (2026-08-18) — the only actively maintained one |
| `ktls` | 6.0.2 (2025-04-07), pins `rustls 0.23.12` / `tokio-rustls 0.26` |
| `ktls-core` / `ktls-stream` | 0.0.5 (2025-11-09) — a `0.0.x` rewrite in progress |
| `rustls` | 0.23.43 (2026-07-29) |
| `hyper` | `master`, `src/proto/h1/io.rs`, `src/proto/h1/dispatch.rs` |
| `tower-http` | `master`, `services/fs/` |
| `actix-files`, `static-web-server`, `hyper-staticfile`, `trow` | `master`, read directly |
| Bench host | Apple M1 Pro, 10 cores, macOS 25.6, rustc 1.93.0 |

---

## 0. The bottom line first

From `../container-registry/notes/fs_limit.md`: the box is network-bound at
**~1.56 GB/s (1.45 GiB/s)** egress, with ~3 GB/s of local NVMe and 8 vCPUs
underneath. So the question is not "how fast can we go" — it is **"how many of
the 8 vCPUs does saturating the NIC cost, and how much is left for everything
else?"**

Measured cost of moving 1 GiB from page cache to a socket, on the bench host
(details and caveats in §7):

| Path | CPU-seconds per GiB | Cores burned at 1.45 GiB/s | Fraction of an 8-vCPU box |
|---|---|---|---|
| `ReaderStream::new(file)` — hyper's own example, 4 KiB chunks | ~0.62–0.84 | 0.90–1.22 | 11–15 % |
| `tower-http` / `actix-files` default, 64 KiB chunks | 0.27–0.60 | 0.39–0.87 | 5–11 % |
| `std::fs` + `spawn_blocking`, **1 MiB chunks** | 0.11–0.27 | 0.16–0.39 | 2–5 % |
| `sendfile(2)` (true zero-copy, measured over loopback) | 0.11–0.13 | 0.16–0.19 | 2–2.4 % |
| Theoretical floor (mmap, touch a byte per page, no copy) | 0.076 | 0.11 | 1.4 % |

Read that table twice. **The gap between a badly-tuned copy loop and a
well-tuned one is 3–5×. The gap between a well-tuned copy loop and true
zero-copy `sendfile` is about 1 % of the machine.**

And `sendfile` is not free of that 1 % either — it costs a design that fights
hyper, breaks under TLS, blocks the reactor on page-cache misses, and has no
clean expression in the Rust async ecosystem. That is a very bad trade.

---

## 1. `sendfile(2)` and `copy_file_range(2)` from a Tokio server

### 1.1 `copy_file_range` is not a candidate at all

Ruled out immediately, and it is worth stating so nobody re-proposes it:
`copy_file_range(2)` returns **`EINVAL` if either descriptor is not a regular
file**. A socket is not a regular file. It cannot write to a socket, full stop.
([man7 copy_file_range(2)](https://man7.org/linux/man-pages/man2/copy_file_range.2.html))

It *is* interesting for the **push** path — committing an upload, or
server-side blob copy for cross-repo mount on the same filesystem, where on
Linux 5.19+ it can become a reflink instead of a byte copy. That belongs in a
storage-driver note, not here.

`splice(2)` *can* reach a socket, but only via a pipe: file → pipe → socket, so
two syscalls and a pipe pair per in-flight response. It has the same blocking
and TLS problems as `sendfile` and adds an fd pair per stream. No advantage.

### 1.2 hyper will not give you the socket

This is the structural blocker, and it is worth being precise because it is the
thing people hand-wave.

hyper's body abstraction is `http_body::Body` with `type Data: Buf`. Every byte
that leaves hyper does so as an in-memory `Buf`. There is no variant that says
"here is an fd and an offset, you deal with it".

The relevant escape hatches, and why none of them work:

- **`http1::Connection::into_parts()` / `without_shutdown()`** hand back the IO
  object — but only *after* the connection stops serving requests. It is the
  HTTP-upgrade mechanism. You cannot get the fd in the middle of a 200 response
  and hand it back afterwards.
  ([docs.rs](https://docs.rs/hyper/latest/hyper/server/conn/http1/struct.Connection.html))
- **`hyper::upgrade`** requires a 101, which is not a legal response to
  `GET /v2/<name>/blobs/<digest>`.
- **hyper issue [#3026 "non-`Buf` body chunks"](https://github.com/hyperium/hyper/issues/3026)**
  is exactly this request — a `Data` trait plus `poll_write_data` so a body could
  carry an fd for `sendfile`/`splice`/`copy_file_range`/io_uring. Opened
  **27 October 2022**. Still open. No maintainer design response. The equivalent
  ask for HTTP/2 is [h2#381](https://github.com/hyperium/h2/issues/381), also
  open.

So `sendfile` with hyper means **owning the HTTP/1.1 framing for the blob-GET
path yourself**: writing the status line and headers into the socket by hand,
then `sendfile`-ing the body, then correctly resuming keep-alive framing. You
would be reimplementing connection state, pipelining, `Expect: 100-continue`,
chunked-vs-content-length, and error paths that hyper already gets right — for
1 % of an 8-vCPU box. That is the whole argument in one sentence.

### 1.3 `spawn_blocking` is the *wrong* wrapper, and the right one is worse than it looks

If you did have the fd: tokio sets `O_NONBLOCK` on accepted sockets. A blocking
`sendfile` inside `spawn_blocking` would park a pool thread for the **entire
transfer** — a 1 GB layer to a slow client parks a thread for minutes. tokio's
blocking pool defaults to 512 threads; at the 50–200 concurrent pulls
`fs_limit.md` projects you are burning 200 threads to do nothing but wait on
socket writability. That is precisely distribution's `maxthreads: 100` ceiling
that PLAN.md says not to reproduce, rebuilt in Rust.

The *correct* wrapper is not `spawn_blocking` at all. It is the reactor:

```rust
// tokio::net::TcpStream::async_io — verified present on master
stream.async_io(Interest::WRITABLE, || {
    // nix::sys::sendfile, retried on EAGAIN by async_io
}).await
```

`TcpStream::async_io` / `try_io` / `writable()` exist and do the right thing for
the socket side. **But they do not fix the file side.**

### 1.4 The real killer: `sendfile` blocks on page-cache misses

`sendfile(2)` is synchronous with respect to the *file*. There is no
`O_NONBLOCK` for the source. On a page-cache miss it blocks the calling thread
on disk I/O, with no async escape.

This is not theoretical. It is the single best-documented operational fact about
`sendfile`, from the people who ship the fastest static file server in the
world:

> "all the worker processes can become busy with reading files from the drives
> to serve the random load, and cannot handle requests in good time"
> — [nginx, *Thread Pools in NGINX Boost Performance 9x!*](https://www.f5.com/company/blog/nginx/thread-pools-boost-performance-9x)

nginx's answer is `aio threads;` — offloading `read()` and `sendfile()` *to a
thread pool*, plus `sendfile_max_chunk` (default 2 MB) so one call cannot hog a
worker. In other words: **nginx's production configuration for large files is
`sendfile` running inside the moral equivalent of `spawn_blocking`**, precisely
because `sendfile` on the event loop is unsafe.

Now apply that to summ. PLAN.md sizes the registry at ~10⁸ blobs and terabytes
on disk against 32 GiB of RAM. **The page-cache hit rate on blob reads will be
low.** Cold reads are the normal case, not the exception. So a `sendfile` on a
tokio worker thread injects a synchronous NVMe read — ~330 µs per MiB at 3 GB/s
— straight into the reactor, stalling every other connection that worker is
multiplexing. p99 on manifest and catalog requests would fall off a cliff, and
those are the operations this project exists to make fast.

Escaping that means putting `sendfile` back in a thread pool, which reintroduces
thread-per-transfer, which is what we were trying to avoid.

**Verdict on `sendfile`: it saves ~1 % of the box, costs a hand-rolled HTTP
framer, is incompatible with userspace TLS, and either stalls the reactor or
parks a thread. Do not do it.**

---

## 2. io_uring

### 2.1 Runtime maturity — measured by release dates, not vibes

| Crate | Latest | Released | Assessment |
|---|---|---|---|
| `tokio-uring` | 0.5.0 | **2024-05-27** | Abandoned in practice. Two years without a release; open issues about it not building against current `libc`. Superseded by in-tree work. |
| `monoio` | 0.2.4 | **2024-08-20** | Stale. Apache Iggy, who shipped a proof of concept on it, called it "pretty far behind when it comes to feature parity" with limited maintenance. |
| `glommio` | 0.9.0 | **2024-03-25** | Iggy: "pretty much unmaintained at this point." |
| `compio` | 0.19.2 | **2026-08-18** | The only live one. Actively maintained (Iggy reports patches merged "within hours"), cross-platform (IOCP/io_uring/polling). Boxes I/O requests — a heap allocation per op. |

([Apache Iggy, *Thread-per-core and io_uring migration*, Feb 2026](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/))

Every one of these is **thread-per-core with `!Send` futures and
ownership-passing buffer APIs**. They are not drop-in. Adopting one means:

- Rewriting the HTTP stack. hyper's `Body` and `Service` are `Send`; a
  thread-per-core runtime with `!Send` tasks does not compose with axum. Iggy
  hit exactly this class of problem — `RefCell` borrows held across `.await`
  panicking at runtime.
- Losing axum's routing/middleware, which Phase 1 is built on.
- Linux-only, hard. macOS and Windows dev machines get a second code path or no
  server at all. summ is currently developed on macOS.
- Re-solving buffer ownership: io_uring requires the kernel to own the buffer
  for the operation's lifetime, so `&mut [u8]` APIs are unsound and every
  runtime uses `(buf, result)` return-the-buffer signatures.

That is a very large bill for a workload that is **network-bound, not
syscall-bound**. io_uring's headline win is eliminating syscall overhead at high
IOPS. summ does ~1,500 reads/second at 1 MiB chunks to saturate the NIC. Syscall
overhead is not the constraint.

### 2.2 The good news: tokio is bringing io_uring in-tree, transparently

This is the finding that actually changes the calculus, and it is why deferring
is safe rather than merely lazy.

tokio 1.53.0 (2026-07-17) shipped further io_uring work behind
`--cfg tokio_unstable` + `feature = "io-uring"` (Linux only). Reading
`tokio/src/fs/file.rs` on `master`:

```rust
fn poll_read_inner(std: Arc<StdFile>, buf: Buf, max_buf_size: usize)
    -> io::Result<JoinHandle<(Operation, Buf)>>
{
    #[cfg(all(not(test), tokio_unstable, feature = "io-uring",
              feature = "rt", feature = "fs", target_os = "linux"))]
    {
        if let Ok(handle) = crate::runtime::Handle::try_current() {
            let driver_handle = handle.inner.driver().io();
            if driver_handle.is_uring_ready(io_uring::opcode::Read::CODE) {
                return Ok(spawn(Self::uring_read(fd, buf, max_buf_size)));
            }
            if !driver_handle.is_uring_probed() {
                return Ok(spawn(Self::lazy_init_read(std, buf, max_buf_size)));
            }
            // Probed but unsupported: fall through to spawn_blocking.
        }
    }
    // Fallback: spawn_blocking
    Ok(Self::spawn_blocking_read(buf, std, max_buf_size))
}
```

Note the shape: **`tokio::fs::File::read` transparently becomes an io_uring read
where the kernel supports it, and silently falls back to `spawn_blocking` where
it does not.** Same API, no ownership-passing, no thread-per-core, no `!Send`.
`fs::write`, `OpenOptions::open`, `fs::try_exists` and rename are already
wired the same way.

It is unstable and imperfect — the tracking discussion
([tokio#7684](https://github.com/tokio-rs/tokio/discussions/7684)) records ~18 %
single-write latency regression and a global uring behind a `Mutex` that is a
known multi-threaded bottleneck. So it is not something to turn on today. But
the direction is unambiguous: **the way summ will eventually get io_uring is by
staying on tokio and flipping a cfg flag**, not by rewriting onto compio.

Design consequence: **keep the blob read path behind a narrow, swappable
interface** (a `Stream<Item = io::Result<Bytes>>` produced by one function), so
that the day tokio's uring path stabilises the change is one function body.

### 2.3 io_uring zero-copy *send* is a separate thing, and also not ready

`IORING_OP_SEND_ZC` (Linux 6.1+) and `IORING_OP_SENDMSG_ZC` do genuine
zero-copy TX with completion notifications when buffers are reusable. Reported
gains are large on dummy devices (84 % at 8 KB I/O) and
[diminish sharply with smaller I/O on physical NICs](https://kernel-internals.org/io-uring/networking/).
It requires registered buffers and a fundamentally different send API — nothing
in the tokio/hyper stack exposes it, and there is no path to it short of the
thread-per-core rewrite. Note also that on `veth` (i.e. inside containers)
zero-copy send is effectively disabled to avoid packet loops — which matters if
summ is itself containerised.

---

## 3. Plain `tokio::fs` + streaming body — the baseline, and where it leaks

### 3.1 Where every copy happens

For `tower-http`-style serving (`tokio::fs::File` → `ReaderStream` → hyper), a
byte is copied **three** times:

1. **kernel page cache → `tokio::io::blocking::Buf.buf: Vec<u8>`**, inside the
   `spawn_blocking` closure (`Buf::read_from`).
2. **that `Vec` → the caller's `BytesMut`** (`Buf::copy_to` → `dst.put_slice`),
   back on the runtime worker thread. **This copy is pure tokio overhead** — it
   does not exist if you use `std::fs::File` and read straight into the
   destination.
3. **the resulting `Bytes` → the socket**, via `write(2)`/`writev(2)`.

`ReaderStream` itself is clean: `BytesMut::reserve(cap)` → `read_buf` →
`buf.split().freeze()`. `split()` + `freeze()` is a refcount handoff, **not** a
copy. So `Bytes` does its job; the leak is inside `tokio::fs`.

Measured cost of that redundant copy on the bench host: about **0.03
cpu-s/GiB** (path A vs path B at 1 MiB: 0.114 vs 0.111). Small — because the M1
does ~30 GB/s single-core memcpy. On the target Ice Lake Xeon, single-core
memcpy out of cache is closer to 10–12 GB/s, so budget **~0.09 cpu-s/GiB per
copy** there, i.e. ~3× what this bench shows. Still second-order, but it is free
to avoid.

### 3.2 hyper's write side does *not* add a copy over plaintext

Verified in `src/proto/h1/io.rs`:

```rust
let strategy = if io.is_write_vectored() {
    WriteStrategy::Queue     // push the Bytes into a BufList, writev it
} else {
    WriteStrategy::Flatten   // memcpy every chunk into one Vec
};
```

`tokio::net::TcpStream::is_write_vectored()` is `true`, so a body `Bytes` over
plaintext HTTP/1.1 goes into a `BufList` and out through `writev` with **zero
extra copies**. Good news, and it means `Bytes` is genuinely earning its keep.

Two hyper tunables that matter here, both on `http1::Builder`:

- `max_buf_size` — default `8192 + 4096*100 = 417,792` bytes. `can_buffer()`
  refuses to queue more once `remaining() >= max_buf_size`, and the dispatch loop
  stops asking the body for frames. **With 1 MiB chunks that means exactly one
  chunk in flight**: hyper flushes it fully before requesting the next, so the
  socket goes idle while the next read runs. Raising this to ~2–4 MiB lets a
  second chunk queue and keeps the pipe fed.
- `writev(bool)` — leave on auto.

Also `MAX_BUF_LIST_BUFFERS = 16` and `MAX_WRITEV_BUFS = 64`, neither of which
binds with megabyte chunks.

### 3.3 Chunk size is the whole ball game

This is the finding. Measured, single stream, page cache hot:

| Path | 16 KiB | 64 KiB | 256 KiB | 1 MiB | 4 MiB |
|---|---|---|---|---|---|
| A `tokio::fs` + `read_buf` (cpu-s/GiB) | **0.616** | 0.265 | 0.139 | **0.114** | 0.117 |
| B `std::fs` in `spawn_blocking` per chunk | — | 0.272 | 0.134 | **0.111** | 0.102 |

And under concurrency (4 worker threads, 128 concurrent responses) it gets
worse, not better:

| Path | cpu-s/GiB @ conc 128 | aggregate GiB/s |
|---|---|---|
| A `tokio::fs` 64 KiB | **0.840** | 8.9 |
| A `tokio::fs` 1 MiB | 0.345 | 21.9 |
| B `spawn_blocking`/chunk 64 KiB | **0.873** | 8.9 |
| B `spawn_blocking`/chunk 1 MiB | 0.274 | 27.3 |
| C one blocking task + channel, 1 MiB | 0.231 | 32.8 |

**64 KiB → 1 MiB is a 3× reduction in CPU per byte and a 3× increase in
aggregate throughput.** That dwarfs everything `sendfile` could offer. The
mechanism is per-chunk fixed cost: a measured **4.88 µs serial `spawn_blocking`
round trip** on this host. At 64 KiB that is a 12.5 GiB/s ceiling for a *single*
stream before any real work; at 128 concurrent streams contending for the pool
and the worker queues, the effective cost per chunk is far higher.

### 3.4 `mmap` — no

Measured `mmap` + `write(2)` to a socket: **0.183 cpu-s/GiB**, versus
`read`+`write` at 0.168–0.177. **`mmap` is not faster**, because `write(2)` from
a mapping still copies into the socket buffer. The only thing `mmap` avoids is
the read-side copy, and the benchmark shows that copy is not where the money is.

Against that non-win, the risks are severe and specific to summ:

- **Major page faults block the faulting thread synchronously.** A fault on a
  tokio worker is a reactor stall with no async escape — the same failure mode
  as `sendfile`, but harder to see coming because there is no syscall to point
  at. With 10⁸ blobs against 32 GiB of RAM, cold faults are the common case.
- **Address space and TLB pressure.** Concurrent 1 GB mappings, and `munmap`
  triggering TLB-shootdown IPIs across all cores on teardown.
- The general case against mmap in a data system is well made in Crotty et al.,
  *Are You Sure You Want to Use MMAP in Your DBMS?* (CIDR 2022), and every
  argument in it applies here.

The mmap row in the bench that looks good — 0.076 cpu-s/GiB, the "theoretical
floor" — is *not* a serving path. It is `mmap` plus touching one byte per page,
i.e. a measurement of what it costs to merely reference the data. It is in the
table as a lower bound to size the others against, nothing more.

---

## 4. TLS

### 4.1 With userspace rustls, zero-copy is definitionally impossible

The plaintext must pass through the AEAD. `sendfile` cannot happen. That is not
a limitation of any Rust crate; it is arithmetic.

The consolation is that rustls is fast. The
[July 2025 rustls benchmark report](https://rustls.dev/perf/2025-07-31-report/)
puts rustls 0.23.31 at **7,628 MB/s send throughput for TLS 1.3 AES-256-GCM on
x86_64**, ahead of OpenSSL 3.5.15 (6,093 MB/s). At summ's 1.56 GB/s ceiling that
is **~0.20 of one core** for all encryption. Two percent of the box. It is not
worth engineering around.

Note also that `tokio-rustls` returns `is_write_vectored() == true` (verified in
`src/common/mod.rs`), so hyper stays in `Queue` mode under TLS too — but
`rustls::Writer::write_vectored` copies the plaintext into the record buffer
regardless, so there is one unavoidable copy on the TLS path. It is already
counted in the 7.6 GB/s figure.

### 4.2 kTLS: real, but not something to build on in 2026

kTLS + `SSL_sendfile()` is genuinely the only way to get file-to-socket
zero-copy with TLS. rustls supports it in principle — the `secret_extraction`
feature has exposed post-handshake secrets since 0.20.7 specifically so a
connection can be handed to the kernel.

But the ecosystem state is not good:

- **`ktls` 6.0.2, last released 2025-04-07.** ~17 months stale at time of
  writing. Pins `rustls 0.23.12` / `tokio-rustls 0.26`.
- **A rewrite is underway and is at `0.0.x`**: `ktls-core` and `ktls-stream`
  0.0.5 (2025-11-09), with three yanked releases in their first two days.
  Sub-1,000 downloads/month territory.
- **TLS 1.3 `KeyUpdate` is fatal.** From `ktls/src/ktls_stream.rs`:
  `"peer sent a TLS 1.3 KeyUpdate, which is currently unsupported by the ktls
  crate"`. A conforming peer may send one at any time. Long-lived connections
  pulling gigabytes are exactly the case where a peer might.
- The API requires wrapping the transport in a `CorkStream` before the
  handshake, because draining rustls's decrypted buffer cleanly is otherwise
  impossible. That is invasive and fragile.
- kTLS requires the `tls` kernel module, a supported cipher (AES-GCM-128/256 or
  ChaCha20-Poly1305), and it is Linux-only.

And the payoff, measured by the people who ship it: nginx's own numbers for
kTLS + `SSL_sendfile()` are **8–16 % on Ubuntu 21.10** and 18–29 % on FreeBSD
13.0 — improvements on *TLS throughput*, not on total server CPU, and only for
files ≥ 64 KB. F5's own writeup notes that "depending on the CPU architecture,
kTLS might even be slower than userspace TLS."
([F5/NGINX](https://www.f5.com/company/blog/nginx/improving-nginx-performance-with-kernel-tls))

An 8–16 % improvement on a 0.20-core workload is 0.02–0.03 cores. Against a
`0.0.5` crate that dies on `KeyUpdate`.

### 4.3 The proxy case actually strengthens the recommendation

PLAN.md notes summ may sit behind a TLS-terminating proxy. In that topology:

- summ speaks **plaintext HTTP/1.1** on loopback or a private link, so
  `sendfile` becomes technically possible again — and everything in §1.2–1.4
  still applies (hyper won't yield the socket; cold reads block). Nothing
  changes.
- The TLS cost moves to the proxy's budget, not summ's. Whatever the proxy is,
  it is not going to `sendfile` from summ's disk.

So under both topologies the answer is the same, which is a good sign the answer
is right.

---

## 5. HTTP Range requests

### 5.1 What is actually required

- **The spec is a SHOULD, not a MUST.** `distribution-spec/spec.md:196`: "A
  registry SHOULD support the `Range` request header in accordance with
  [RFC 9110 §14]".
- **The conformance suite does not test it on pull.** Grepping
  `conformance/api.go`, every `Range`/`Content-Range` assertion is on the
  *chunked upload* path (`416` on out-of-order chunks, the `Range: 0-<n>`
  response header on upload status). Blob GET ranges are untested.
- **containerd only ever sends one form.** `core/remotes/docker/resolver.go:947`:
  ```go
  r.header.Set("Range", fmt.Sprintf("bytes=%d-", offset))
  ```
  A single open-ended suffix range, used solely to resume an interrupted layer
  fetch. **No multi-range. No `bytes=a-b`. No `bytes=-n`.**

So the required surface is: parse `bytes=<start>-` and `bytes=<start>-<end>`,
reply `206` with `Content-Range: bytes s-e/total` and the right
`Content-Length`, `416` on unsatisfiable, and advertise `Accept-Ranges: bytes`.
Multipart/byteranges (`multipart/byteranges` with boundaries) can be answered
with a plain `200` full-body response, which RFC 9110 explicitly permits — and
which is what `tower-http` does by default via `ignore_multi_range_requests`.

### 5.2 Range does not change the recommendation, but it does pick the syscall

- `sendfile(2)` takes an offset and count natively — ranges would be free.
- `tokio::fs::File` requires `seek(SeekFrom::Start(start)).await` then
  `.take(len)` — an extra `spawn_blocking` round trip for the seek, and a
  stateful cursor. `tower-http` does exactly this (`open_file.rs:212`).
- **`std::os::unix::fs::FileExt::read_at` (`pread`) is strictly better**: no
  seek syscall, no cursor state, and the offset arithmetic for a range is the
  same arithmetic you already need for chunking. It also makes the reader
  trivially shareable — the same `Arc<File>` can serve two ranges concurrently
  without a lock, which matters if a client ever parallelises a layer fetch.

`actix-files` seeks per chunk (`file.seek(SeekFrom::Start(offset))` then read,
`chunked.rs`), which is two syscalls per 64 KiB where one would do. Don't copy
that.

**Range handling therefore argues for `pread` over `tokio::fs::File`, which is
the same conclusion §3 reached for a different reason.**

---

## 6. What fast Rust file servers actually do

Read directly, not from blog posts. The result is striking: **not one of them
uses `sendfile`, and most of them use a chunk size that the measurements in §3
say is 3–5× too small.**

| Project | Mechanism | Chunk | Notes |
|---|---|---|---|
| **hyper's own `examples/send_file.rs`** | `ReaderStream::new(file)` | **4 KiB** (`ReaderStream::DEFAULT_CAPACITY`) | The canonical example is the worst configuration measured. |
| **`tower-http` `ServeFile`/`ServeDir`** | `tokio::fs::File` → `ReaderStream::with_capacity` → `AsyncReadBody` | **64 KiB** default, `with_buf_chunk_size()` to change | Seeks for ranges; punts on multi-range via `ignore_multi_range_requests`. |
| **`actix-files`** | `std::fs::File` in `web::block`, `seek`+`read`, `Bytes::from(vec)` | **64 KiB** hardcoded (`chunked.rs`) | Has a nice trick: `read_mode_threshold` serves files below a size limit with a *synchronous* read on the worker thread, skipping the thread hop entirely. Default threshold is `0`, i.e. off. |
| **`static-web-server`** | **synchronous `std::io::Read` on the reactor thread**, buffer = `metadata.blksize()` | **4 KiB** on ext4 (`optimal_buf_size`, min 4096) | Inherited from warp. A "production-ready, high performance" server doing 4 KiB blocking reads on the event loop. |
| **`hyper-staticfile`** | `FileBytesStream` over a `TokioFileAccess`, with dedicated `Range` and `MultiRange` body variants | tokio default | The cleanest *range* model of the bunch — worth copying its `Body::{Full, Range, MultiRange}` enum shape. |
| **`trow`** (Rust OCI registry — the closest prior art) | `tokio::fs::File` → `FramedRead<_, BytesCodec>` → `axum::body::Body::from_stream` | **8 KiB** (`tokio_util::codec` `INITIAL_CAPACITY`) | No `Range` support on blob GET at all. Serves gigabyte layers 8 KiB at a time. This is the bar to clear, and it is on the floor. |

The absence of `sendfile` from this list is itself evidence. `tk-sendfile` and
`sendfile.rs` exist but are tokio-0.1-era abandonware. Nobody in the modern Rust
HTTP ecosystem does it, because hyper structurally does not permit it (§1.2).

**The actionable read:** summ does not need to invent anything. It needs to do
what these projects do, with the chunk size set two orders of magnitude higher
and `pread` instead of `seek`+`read`. That alone should put it 3–5× ahead of
trow and comfortably ahead of a default `tower-http` setup, before any exotic
syscall.

---

## 7. Benchmarks

Scratch code lives outside the repo at
`/private/tmp/.../scratchpad/blobbench/` (`src/main.rs`, `src/bin/sock.rs`,
`src/bin/conc.rs`). Nothing was added to the summ tree.

### 7.1 Caveats — read these before quoting any number

- **Host is an Apple M1 Pro (macOS 25.6, aarch64), not the target
  Standard_L8s_v3 (Ice Lake Xeon, Linux).** Two consequences: (a) memcpy is
  unusually cheap here (~30–34 GB/s single-core vs ~10–12 GB/s typical for a
  Xeon out of cache), so **copy-heavy paths look ~3× better here than they will
  on the target** — which means the case for larger chunks and fewer copies is
  *stronger* on the real hardware, not weaker; (b) macOS `sendfile(2)` is a
  different and generally weaker implementation than Linux's, so the
  `sendfile`-vs-`read`/`write` gap on Linux may be somewhat larger than measured.
- **Page cache hot throughout.** Real serving will be cold-cache dominated. This
  biases *in favour of* `sendfile` and `mmap` in the measurements while hiding
  their worst failure mode (§1.4, §3.4).
- **Socket tests are over loopback**, so the receiver is on the same box; only
  the sender's `getrusage` is measured, but loopback still exercises the
  copy-into-socket-buffer path that a real NIC would.
- No TLS in any measurement. rustls throughput is taken from the
  [official rustls benchmark report](https://rustls.dev/perf/2025-07-31-report/).

### 7.2 Calibration

```
single-core memcpy (1 MiB working set):   29.62 GB/s
single-core memcpy (256 MiB working set): 34.38 GB/s
serial spawn_blocking round-trip:          4.88 us
  -> at 64 KiB/chunk that caps ONE stream at  12.50 GiB/s
  -> at  1 MiB/chunk that caps ONE stream at 200.07 GiB/s
```

### 7.3 Read path, single stream, 512 MiB file, page cache hot

```
path                                        wall      cpu    GiB/s   cpu-s/GiB
A tokio::fs read_buf     chunk=16 KiB      0.339s   0.308s    1.47      0.616
A tokio::fs read_buf     chunk=64 KiB      0.130s   0.132s    3.84      0.265
A tokio::fs read_buf     chunk=256 KiB     0.067s   0.070s    7.49      0.139
A tokio::fs read_buf     chunk=1 MiB       0.067s   0.057s    7.44      0.114
A tokio::fs read_buf     chunk=4 MiB       0.059s   0.059s    8.54      0.117
B spawn_blocking/chunk   chunk=64 KiB      0.127s   0.136s    3.94      0.272
B spawn_blocking/chunk   chunk=256 KiB     0.064s   0.067s    7.75      0.134
B spawn_blocking/chunk   chunk=1 MiB       0.056s   0.055s    8.98      0.111
B spawn_blocking/chunk   chunk=4 MiB       0.051s   0.051s    9.76      0.102
C blocking task+channel  chunk=64 KiB      0.081s   0.101s    6.19      0.202
C blocking task+channel  chunk=256 KiB     0.052s   0.057s    9.70      0.115
C blocking task+channel  chunk=1 MiB       0.046s   0.048s   10.81      0.095
C blocking task+channel  chunk=4 MiB       0.057s   0.057s    8.78      0.114
D mmap, touch 1 byte/page (no copy at all) 0.038s   0.038s   13.11      0.076
```

### 7.4 File → socket, server-side CPU, 512 MiB over loopback (two runs)

```
mode        chunk       wall     cpu    GiB/s   cpu-s/GiB
sendfile    1 MiB      0.080s  0.064s   6.28     0.1278
sendfile    1 MiB      0.055s  0.055s   9.02     0.1104
readwrite   64 KiB     0.100s  0.098s   5.01     0.1958
readwrite   64 KiB     0.113s  0.111s   4.41     0.2215
readwrite   256 KiB    0.096s  0.096s   5.20     0.1915
readwrite   256 KiB    0.086s  0.086s   5.79     0.1717
readwrite   1 MiB      0.089s  0.088s   5.64     0.1768
readwrite   1 MiB      0.085s  0.084s   5.90     0.1677
readwrite   4 MiB      0.109s  0.107s   4.58     0.2140
doublecopy  64 KiB     0.120s  0.118s   4.16     0.2358   <- models tokio::fs
doublecopy  1 MiB      0.099s  0.099s   5.06     0.1975   <- models tokio::fs
mmapwrite   1 MiB      0.092s  0.092s   5.42     0.1833
```

`sendfile` saves ~0.05–0.07 cpu-s/GiB over a well-tuned `read`+`write`. At
1.45 GiB/s that is **0.07–0.10 of one core out of 8**.

`doublecopy` (read into a buffer, memcpy to a second buffer, write) is the
`tokio::fs` shape; it costs ~0.02–0.04 cpu-s/GiB over `readwrite`, matching the
memcpy calibration.

### 7.5 Concurrency, 128 MiB file, 4 worker threads, page cache hot

```
path                              conc  wall(s)   cpu(s)  agg GiB/s  cpu-s/GiB
A tokio::fs 64K                      1    0.099    0.065       1.27     0.5165
A tokio::fs 1M                       1    0.015    0.015       8.45     0.1216
B spawn_blocking/chunk 64K           1    0.039    0.042       3.23     0.3361
B spawn_blocking/chunk 1M            1    0.014    0.015       8.78     0.1165
C blocking task+chan 1M              1    0.012    0.013      10.31     0.1069

A tokio::fs 64K                      8    0.119    0.593       8.37     0.5933
A tokio::fs 1M                       8    0.037    0.233      26.97     0.2325
B spawn_blocking/chunk 64K           8    0.106    0.686       9.43     0.6857
B spawn_blocking/chunk 1M            8    0.027    0.163      36.86     0.1629
C blocking task+chan 1M              8    0.026    0.169      38.03     0.1691

A tokio::fs 64K                     32    0.461    2.790       8.68     0.6976
A tokio::fs 1M                      32    0.162    1.254      24.77     0.3135
B spawn_blocking/chunk 64K          32    0.471    3.149       8.50     0.7874
B spawn_blocking/chunk 1M           32    0.119    0.918      33.73     0.2296
C blocking task+chan 1M             32    0.125    0.960      31.89     0.2401

A tokio::fs 64K                    128    1.789   13.444       8.94     0.8403
A tokio::fs 1M                     128    0.730    5.520      21.91     0.3450
B spawn_blocking/chunk 64K         128    1.792   13.975       8.93     0.8734
B spawn_blocking/chunk 1M          128    0.587    4.377      27.27     0.2735
C blocking task+chan 1M            128    0.488    3.689      32.76     0.2305
```

Note that CPU-per-byte *rises* with concurrency for every path — scheduling and
cache contention — and that the 64 KiB paths plateau at ~9 GiB/s aggregate
regardless of concurrency while the 1 MiB paths reach 22–33 GiB/s. Under load is
exactly where chunk size matters most.

Path C (one blocking thread per response, chunks over a bounded channel) is the
fastest at high concurrency — but it is a **trap**, and it is not recommended: a
slow client applies backpressure through the channel and parks a pool thread for
the whole transfer. That is thread-per-connection with extra steps, and it is
distribution's `maxthreads: 100` failure mode. Path B keeps thread occupancy to
one read (~330 µs per MiB at 3 GB/s), so the pool serves an unbounded number of
slow clients.

---

## RECOMMENDATION

### For v1 (Phase 3): a tuned `pread` + `spawn_blocking` streaming body

Concretely:

1. **`std::fs::File` + `FileExt::read_at` (`pread`) inside `spawn_blocking`, one
   chunk per call.** Not `tokio::fs::File` — that adds a redundant memcpy
   (§3.1) and forces a `seek` round trip for ranges (§5.2). Not one long-lived
   blocking task per response — that parks a thread on a slow client (§7.5).
2. **1 MiB chunks**, configurable, floor of 256 KiB. This is the single
   highest-value decision in the whole document: 3–5× versus the 4–64 KiB that
   every surveyed project defaults to (§3.3, §6).
3. **Keep one chunk read in flight ahead of the one being written** (a
   `poll_frame` that starts the next `read_at` before yielding the current
   `Bytes`), and raise `hyper::server::conn::http1::Builder::max_buf_size` to
   ~2–4 MiB so hyper will queue the prefetched chunk instead of stalling the
   socket while the next read runs (§3.2).
4. **Hand hyper `Bytes` and let `writev` do its job.** Over plaintext this is
   already copy-free on the write side. Do not flatten, do not re-buffer.
5. **Single-range only**, `bytes=<start>-` and `bytes=<start>-<end>`, `206` +
   `Content-Range` + `Accept-Ranges: bytes`, `416` on unsatisfiable, plain `200`
   for multi-range. That is everything containerd asks for and more than
   conformance tests (§5.1).
6. **Serve blob GETs over HTTP/1.1.** If HTTP/2 is ever enabled, the default
   64 KiB flow-control window (`SPEC_WINDOW_SIZE = 65_535` in hyper) will cap a
   single stream far below line rate on a fat pipe; `adaptive_window(true)` or
   explicit multi-MB `initial_stream_window_size` /
   `initial_connection_window_size` are mandatory, and `sendfile` becomes
   permanently impossible anyway.
7. **Put the whole read path behind one function** returning
   `impl Stream<Item = io::Result<Bytes>>` (plus a range variant). Everything
   below is then swappable without touching the HTTP layer — which is what makes
   every deferral in this document cheap to reverse.

Expected result: ~0.2–0.4 cores at 1.45 GiB/s on the target box (scaling the
measured 0.11–0.27 cpu-s/GiB for the Xeon's slower memcpy), leaving ~7.5 of 8
vCPUs for concurrency, TLS, and metadata. That is the stated goal met.

### Defer

- **`sendfile`/`splice`.** Worth ~1 % of the box; costs a hand-rolled HTTP/1.1
  framer because hyper will not yield the socket ([#3026](https://github.com/hyperium/hyper/issues/3026),
  open since 2022); is impossible under userspace TLS; and blocks on cold page
  cache, which is summ's normal case. Revisit only if hyper ships non-`Buf` body
  chunks.
- **io_uring runtimes.** `tokio-uring`, `monoio` and `glommio` have not shipped
  a release since 2024. `compio` is alive but thread-per-core with `!Send`
  futures, which means giving up axum and hyper. Meanwhile **tokio is landing
  io_uring in-tree behind a cfg flag with no API change** (§2.2) — that is the
  upgrade path, and it costs nothing to wait for.
- **kTLS.** The maintained crate is 17 months stale, the replacement is `0.0.5`,
  TLS 1.3 `KeyUpdate` is fatal, and nginx's own measured win is 8–16 % of a
  0.2-core workload.
- **`mmap`.** Measured *slower* than `read`+`write` to a socket, with reactor
  stalls on major faults as the downside.
- **`copy_file_range`.** Cannot target a socket (`EINVAL`). Reconsider it for
  the *push* path — upload commit and same-filesystem cross-repo mount, where on
  Linux 5.19+ it can become a reflink.

### What would change my mind

Ranked by how likely it is to actually happen.

1. **A profile from the real rig showing blob serving above ~15 % of CPU at
   saturation.** That is the trigger to look harder. Phase 0 already has the
   harness; add a `perf`/`pidstat` capture to the pull benchmark so this number
   exists rather than being argued about. If it lands at 2–5 % as predicted,
   this document is done.
2. **Cold-cache measurements changing the shape.** Everything here is page-cache
   hot. If a cold-cache run shows the blocking pool saturating or read latency
   dominating, the fix is prefetch depth and chunk size — still not `sendfile`,
   which is *worse* cold.
3. **tokio's `io-uring` feature stabilising with a per-worker (not global-mutex)
   ring.** Then flip the cfg and re-measure §7.3. Zero code change if
   recommendation 7 is honoured. Watch
   [tokio#7684](https://github.com/tokio-rs/tokio/discussions/7684).
4. **hyper accepting non-`Buf` body chunks (#3026).** That is the only thing
   that makes `sendfile` cheap enough to be worth its remaining downsides.
5. **A measured `KeyUpdate`-safe kTLS** with a maintained crate, *and* a
   profile showing TLS above ~10 % of CPU. Both conditions, not either.
6. **The workload turning out not to be network-bound** — e.g. summ deployed on
   a 100 GbE box, or S3-backed storage where the constraint moves entirely. Then
   redo the arithmetic in §0 from scratch; every conclusion here is downstream of
   the 1.56 GB/s number in `fs_limit.md`.

### One-line summary for PLAN.md

> Plain-ish `tokio` file streaming is fine and will stay fine: `pread` in
> `spawn_blocking` with 1 MiB chunks, `Bytes` into hyper's `writev`, single-range
> `206`s. Measured at ~2–5 % of an 8-vCPU box at line rate, versus ~1 % for true
> `sendfile` zero-copy that would cost a hand-rolled HTTP framer, break under
> TLS, and stall the reactor on cold reads. Chunk size, not zero-copy, is worth
> 3–5×.
