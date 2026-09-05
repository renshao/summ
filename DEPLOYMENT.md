# Running summ as a systemd service

The binary is self-contained, so a production deployment is a user, a data
directory and a unit file. What follows is the whole procedure.

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
TLS. An example Caddyfile:

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
