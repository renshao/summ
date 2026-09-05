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

## Running as a systemd service

The binary is self-contained, so a production deployment is a user, a data
directory and a unit file. What follows is the whole procedure; it is what runs
the public demo at [demo.registry.summcr.com](https://demo.registry.summcr.com).

### A dedicated user

Give the registry its own unprivileged account rather than running it as a
login user. It is internet-facing and it parses attacker-supplied manifests,
blobs and tag names, so it should be able to write to exactly one directory and
nothing else — and a login user is typically in `sudo` or `docker`, either of
which makes a compromise of the service a compromise of the machine.

```sh
sudo groupadd --system --gid 10001 summ
sudo useradd --system --uid 10001 --gid summ \
  --home-dir /var/lib/summ --shell /usr/sbin/nologin summ
sudo install -d -o summ -g summ -m 0750 /var/lib/summ
```

The uid is pinned to 10001 to match the one the `Dockerfile` creates. That is
not cosmetic: it means a data directory keeps the same ownership whether it is
served by the unit below or bind-mounted into the container image, so the two
deployment paths stay interchangeable instead of quietly disagreeing.

### Install the binary outside `/home`

```sh
sudo install -o root -g root -m 0755 ./summ /usr/local/bin/summ
```

`/usr/local/bin`, not a home directory, because the unit below sets
`ProtectHome=yes` — which makes `/home` empty *inside the service's namespace*,
so an `ExecStart` pointing there fails to start with a confusing `No such file
or directory`. Build wherever you like; install the artefact somewhere the
sandbox can still see it.

### Credentials, if any

`--auth none` is the default and needs no key file. For a registry reachable
from anywhere, `--auth write` is usually the shape you want — anonymous pull so
the catalog and the UI are browsable, a key for push:

```sh
umask 077
cat > /etc/summ.env <<EOF
SUMM_AUTH=write
SUMM_WRITE_APIKEY=$(head -c 32 /dev/urandom | base64 | tr -d '=+/' | cut -c1-40)
EOF
```

Keep this out of any git checkout, or add it to `.gitignore` if it must live in
one. Under `--auth write` a `SUMM_READ_APIKEY` in the same file is a *startup
error* rather than a warning — supplying a key that the mode does not use is
ambiguous between "ignore it" and "infer the mode from it", and both of those
fail silently and leave a registry more open than its operator believes.

### The unit

```ini
# /etc/systemd/system/summ.service
[Unit]
Description=summ container registry
Documentation=https://github.com/summcr/summ
After=network-online.target
Wants=network-online.target
# Only if the data directory is a separate mount — see the note below.
RequiresMountsFor=/var/lib/summ

[Service]
Type=exec
User=summ
Group=summ
EnvironmentFile=/etc/summ.env
Environment=SUMM_LOG=summ=info,summ_server=info,tower_http=info
ExecStart=/usr/local/bin/summ serve --listen 127.0.0.1:5000 --data-dir /var/lib/summ
Restart=always
RestartSec=5

# RocksDB holds many SST files open and each in-flight pull holds a blob fd.
LimitNOFILE=65535

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/summ
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictSUIDSGID=yes
RestrictRealtime=yes
RestrictNamespaces=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
UMask=0077

[Install]
WantedBy=multi-user.target
```

`ReadWritePaths` is the line that earns the dedicated user: everything else on
the filesystem is read-only to the process, so the sandbox is worth having
rather than decorative.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now summ.service
curl -fsS http://127.0.0.1:5000/v2/    # the spec's own liveness probe
```

**If the data directory is its own mount, keep `RequiresMountsFor`.** Without
it a failed mount does not stop the service — summ starts, finds no `meta/`,
creates a fresh empty store in the bare mountpoint on the root filesystem, and
comes up serving an empty catalog while accepting writes onto the wrong disk.
That is a worse failure than not starting, because it looks like it worked.
`meta/` and `blobs/` must also stay on **one** filesystem: an upload is
committed by renaming its staging file into the blob tree, and a rename across
devices is not a rename.

### Behind a reverse proxy

summ speaks plain HTTP and expects to be fronted by something that terminates
TLS. A Caddyfile for the demo host:

```caddyfile
registry.example.com {
	reverse_proxy 127.0.0.1:5000

	# Never re-encode blob bodies: layer tarballs are already compressed, the
	# digest is computed over the plaintext, and the byte path is the one place
	# that has to stay cheap. Manifests, JSON and the UI still compress.
	@compressible not path /v2/*/blobs/*
	encode @compressible zstd gzip

	# Deliberately no request-body size limit. A layer is routinely gigabytes,
	# and summ enforces its own ceiling with --max-upload-bytes (32 GiB by
	# default), where it can reject on Content-Length instead of after writing
	# the body.
}
```

Two things to get right in any proxy, not just this one. Do not put a request
body cap in front of a registry unless it is above your largest layer — no
client chunks a layer, so a cap is the largest image you can push, and the
failure is a `413` that no retry can fix. And do not let the proxy buffer
request bodies to disk or memory; summ streams an upload straight to its
staging file, and a buffering proxy reintroduces the memory cost that design
removes.

One Caddy-specific trap: `caddy validate` *provisions* the configuration, which
opens any file named by a `log` directive. Run it under `sudo` and it creates
that log file owned by `root`, after which the `caddy` user cannot write to it
and the next reload fails with `permission denied` — from a config that just
validated. Either validate as the `caddy` user, or `chown caddy:caddy` the log
file afterwards.
