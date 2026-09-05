# summ Container Registry

[![CI](https://github.com/summcr/summ/actions/workflows/ci.yml/badge.svg)](https://github.com/summcr/summ/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**summ** is a simple yet powerful container registry with batteries included. It
fully supports the OCI Distribution Spec and adds the practical things a
registry should have had all along — image pull statistics, tag history, a
built-in web UI — so it is useful the moment it starts.

summ is written in Rust over a metadata schema designed for discovery rather
than transfer, which is what makes it fast where it counts: on the four serial
metadata lookups a real `docker pull` waits on.

## Features

**Pull counts, per day and per hour.** Every repository, tag and manifest gets a
thirty-day contribution grid and a last-24-hours strip, so "what is anyone
actually pulling" is a page rather than a log-parsing exercise. Serving a pull
never touches the store — a `GET` adds to a map in memory and a background task
folds it into the metadata store every few seconds — so the counters cost the
pull path nothing.

**Tag history.** Every tag mutation has been recorded since the first push, so
you can ask what a tag has pointed at over time *and* what a manifest has ever
been called. Both are the same endpoint, addressed by tag or by digest, newest
first, cursor-paged. History outlives what it describes: a deleted tag still
answers, because "gone" is exactly the question you are asking.

**A built-in web UI.** Same binary, same port, assets compiled in — no build
step, no framework, no CDN, so it works air-gapped. Browse repositories with
per-repo tag and manifest counts, search by name prefix, drill into a manifest,
and see the pull-count grids and tag timelines beside the thing they describe.

**Metadata lookups are the product.** Four of the five serial steps in a cold
`containerd` pull are metadata lookups, and their latencies add — so summ is
built around a purpose-designed key schema over RocksDB rather than around the
byte path. Nothing is a directory walk, no stored value grows with the size of
the registry, and prefix bloom filters make the hot existence checks ~6× faster
than the defaults. Measured: 7.42 GiB layers pushed at ~1.0 GB/s and pulled back
from four concurrent clients at ~1.1 GB/s aggregate.

**Discovery as a first-class API.** `/api/v1/` serves repositories, tags,
manifests, tag history and pull counts as a flat, cursor-paged, read-only
surface. Every list takes a cursor and a limit; the design target is 10M
repositories and up to 10M manifests in a single one, so nothing here
materialises an unbounded set.

**One binary, no dependencies.** RocksDB is compiled in and statically linked.
No database to run, no object store, no sidecar — `./summ serve` is the whole
deployment. Optional API-key auth (`--auth none|write|all`) puts a read key and
a write key in front of the registry, the discovery API and the UI at once.

**Conformant.** The OCI `distribution-spec` conformance suite passes with zero
failures at every profile, including the OCI 1.1 referrers API — 1032 checks
passing at the suite's `dev` profile, with nothing skipped.

## Download

Prebuilt Linux x86_64 binaries are published on the
[releases page](https://github.com/summcr/summ/releases). RocksDB and its C++
runtime are linked in statically, so the only shared libraries left are the
ones every glibc system already has — `libc`, `libm` and `libgcc_s`. The build
floor is glibc 2.34: Ubuntu 22.04, Debian 12, RHEL 9 and newer.

```sh
curl -fsSL https://github.com/summcr/summ/releases/latest/download/summ-x86_64-unknown-linux-gnu.tar.gz \
  | tar -xz summ
./summ serve
```

`dev` is a rolling prerelease built from `main` on demand — swap `latest/download`
for `download/dev` to fetch it.
