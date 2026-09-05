#!/usr/bin/env bash
#
# large-image-e2e.sh — push a large real-world image into a freshly built summ
# and hammer it with concurrent pulls.
#
# The question this answers is "does summ survive a multi-gigabyte image end to
# end": a push whose largest layer is several GiB, a pull of every byte back
# with the digest verified, N of those at once, plus the range shapes containerd
# actually sends. It is deliberately not a benchmark — the throughput numbers it
# prints are a sanity check, not a measurement.
#
#   ./scripts/large-image-e2e.sh                    # defaults: pytorch, 4 threads
#   ./scripts/large-image-e2e.sh --image alpine:3 --threads 8 --rounds 3
#   ./scripts/large-image-e2e.sh --reuse-data --no-build   # re-pull, skip push
#   ./scripts/large-image-e2e.sh --auth --engine redb
#
# Image bytes come from the local docker daemon when it holds the image
# (`docker save` writes an OCI layout, which `oras cp` pushes straight from) and
# from the upstream registry otherwise. Nothing is pushed through the docker
# daemon: on Docker Desktop `127.0.0.1` inside the VM is not the host, so
# `docker push 127.0.0.1:<port>/…` cannot reach a registry running here.
#
# Requires: cargo, oras, curl, jq, shasum. docker only for the local-image path.

set -euo pipefail

# ---------------------------------------------------------------- defaults ---

IMAGE="pytorch/pytorch:2.9.0-cuda12.8-cudnn9-runtime"
DEST_REPO=""                # derived from IMAGE unless given
DEST_TAG=""                 # derived from IMAGE unless given
PORT=15000                  # not 5000: on macOS that is AirPlay Receiver
THREADS=4
ROUNDS=1
ENGINE="rocks"
SOURCE="auto"               # auto | docker | remote
DO_BUILD=1
DO_VERIFY=1
DO_PUSH=1
KEEP=0
REUSE_DATA=0
REFRESH_LAYOUT=0
AUTH=0
DATA_DIR=""
LAYOUT_DIR=""
STATE_ROOT="${TMPDIR:-/tmp}/summ-e2e"

READ_KEY="e2e-read-key"
WRITE_KEY="e2e-write-key"

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed 's/^#\{1,2\} \{0,1\}//; s/^set -euo.*//'
    cat <<'EOF'
Options:
  --image REF          image to exercise (default: pytorch cuda runtime, ~8 GB)
  --repo NAME          destination repository in summ (default: image path)
  --tag TAG            destination tag (default: image tag)
  --port N             listen port (default: 15000)
  --threads N          concurrent pullers (default: 4)
  --rounds N           full pulls per thread (default: 1)
  --engine rocks|redb  metadata engine (default: rocks)
  --source MODE        auto | docker | remote (default: auto)
  --data-dir DIR       registry data directory (default: under $TMPDIR)
  --layout-dir DIR     OCI layout cache for the docker path
  --auth               run with --auth all and use the keys for every request
  --no-build           use target/release/summ as it stands
  --no-verify          do not sha256 pulled blobs (measures raw throughput)
  --reuse-data         keep an existing data dir and skip the push phase
  --refresh-layout     re-export the image even if a cached layout exists
  --keep               leave the data dir and layout in place on exit
  -h, --help           this message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --image)        IMAGE="$2"; shift 2 ;;
        --repo)         DEST_REPO="$2"; shift 2 ;;
        --tag)          DEST_TAG="$2"; shift 2 ;;
        --port)         PORT="$2"; shift 2 ;;
        --threads)      THREADS="$2"; shift 2 ;;
        --rounds)       ROUNDS="$2"; shift 2 ;;
        --engine)       ENGINE="$2"; shift 2 ;;
        --source)       SOURCE="$2"; shift 2 ;;
        --data-dir)     DATA_DIR="$2"; shift 2 ;;
        --layout-dir)   LAYOUT_DIR="$2"; shift 2 ;;
        --auth)         AUTH=1; shift ;;
        --no-build)     DO_BUILD=0; shift ;;
        --no-verify)    DO_VERIFY=0; shift ;;
        --reuse-data)   REUSE_DATA=1; DO_PUSH=0; shift ;;
        --refresh-layout) REFRESH_LAYOUT=1; shift ;;
        --keep)         KEEP=1; shift ;;
        -h|--help)      usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/summ"
REGISTRY="127.0.0.1:$PORT"

# Split the image reference. A registry host is anything before the first `/`
# that looks like a host (has a dot or a colon), which is the same rule the
# clients use; everything else is a docker.io short name.
IMAGE_NO_TAG="${IMAGE%:*}"
IMAGE_TAG="${IMAGE##*:}"
[[ "$IMAGE_TAG" == "$IMAGE" ]] && IMAGE_TAG="latest" && IMAGE_NO_TAG="$IMAGE"
first="${IMAGE_NO_TAG%%/*}"
if [[ "$first" == *.* || "$first" == *:* ]]; then
    SRC_PATH="${IMAGE_NO_TAG#*/}"
else
    SRC_PATH="$IMAGE_NO_TAG"
fi
[[ -n "$DEST_REPO" ]] || DEST_REPO="$SRC_PATH"
[[ -n "$DEST_TAG" ]] || DEST_TAG="$IMAGE_TAG"

SLUG="$(printf '%s' "$IMAGE" | tr '/:@' '___')"
[[ -n "$DATA_DIR" ]] || DATA_DIR="$STATE_ROOT/data"
[[ -n "$LAYOUT_DIR" ]] || LAYOUT_DIR="$STATE_ROOT/layouts/$SLUG"
RUN_DIR="$STATE_ROOT/run"

# ------------------------------------------------------------------ output ---

if [[ -t 1 ]]; then
    B=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GRN=$'\033[32m'; YEL=$'\033[33m'; N=$'\033[0m'
else
    B=""; DIM=""; RED=""; GRN=""; YEL=""; N=""
fi

step()  { printf '\n%s==> %s%s\n' "$B" "$*" "$N"; }
info()  { printf '    %s\n' "$*"; }
note()  { printf '    %s%s%s\n' "$DIM" "$*" "$N"; }
ok()    { printf '    %s✓%s %s\n' "$GRN" "$N" "$*"; }
warn()  { printf '    %s!%s %s\n' "$YEL" "$N" "$*"; }
die()   { printf '\n%serror:%s %s\n' "$RED" "$N" "$*" >&2; exit 1; }

human() { # bytes -> human
    awk -v b="$1" 'BEGIN{
        split("B KiB MiB GiB TiB",u," "); i=1
        while (b>=1024 && i<5) { b/=1024; i++ }
        printf (i==1 ? "%d %s" : "%.2f %s"), b, u[i]
    }'
}
rate() { # bytes seconds -> MB/s
    awk -v b="$1" -v s="$2" 'BEGIN{ if (s<=0) {print "n/a"; exit} printf "%.1f MB/s", b/s/1000000 }'
}
now() { python3 -c 'import time; print(time.time())'; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.1f", b-a}'; }

for tool in curl jq shasum awk python3; do
    command -v "$tool" >/dev/null || die "$tool is required"
done

# ------------------------------------------------------------------- setup ---

SERVER_PID=""
cleanup() {
    local code=$?
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ $KEEP -eq 0 ]]; then
        rm -rf "$DATA_DIR" "$RUN_DIR"
    else
        printf '\n%skept:%s data=%s layout=%s log=%s\n' "$DIM" "$N" \
            "$DATA_DIR" "$LAYOUT_DIR" "$RUN_DIR/summ.log"
    fi
    exit $code
}
trap cleanup EXIT INT TERM

mkdir -p "$STATE_ROOT" "$RUN_DIR"
LOG="$RUN_DIR/summ.log"

# curl arguments shared by every request: fail loudly, follow nothing, and carry
# the credential when auth is on. The key is the *password* of a Basic
# credential; the username is ignored, so any value does.
CURL=(curl -sS --fail-with-body)
if [[ $AUTH -eq 1 ]]; then
    CURL+=(-u "e2e:$WRITE_KEY")
fi

step "Configuration"
info "image      $IMAGE"
info "target     $REGISTRY/$DEST_REPO:$DEST_TAG"
info "engine     $ENGINE   threads $THREADS   rounds $ROUNDS"
info "data dir   $DATA_DIR"
info "verify     $([[ $DO_VERIFY -eq 1 ]] && echo 'sha256 every blob' || echo 'off')"
info "auth       $([[ $AUTH -eq 1 ]] && echo 'all' || echo 'none')"

# ------------------------------------------------------------------- build ---

if [[ $DO_BUILD -eq 1 ]]; then
    step "Building release binary"
    (cd "$REPO_ROOT" && cargo build --release 2>&1 | tail -3)
fi
[[ -x "$BIN" ]] || die "no binary at $BIN (drop --no-build?)"
info "$("$BIN" --version) — $(cd "$REPO_ROOT" && git log -1 --format='%h %s' 2>/dev/null)"

# ------------------------------------------------------------------ server ---

step "Starting summ"
if [[ $REUSE_DATA -eq 0 ]]; then
    rm -rf "$DATA_DIR"
elif [[ ! -d "$DATA_DIR" ]]; then
    die "--reuse-data given but $DATA_DIR does not exist"
fi
mkdir -p "$DATA_DIR"

SERVE_ARGS=(serve --listen "$REGISTRY" --data-dir "$DATA_DIR" --engine "$ENGINE")
if [[ $AUTH -eq 1 ]]; then
    SERVE_ARGS+=(--auth all --read-apikey "$READ_KEY" --write-apikey "$WRITE_KEY")
fi
"$BIN" "${SERVE_ARGS[@]}" >"$LOG" 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 60); do
    if [[ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "http://$REGISTRY/v2/" 2>/dev/null || true)" == "200" ]]; then
        break
    fi
    kill -0 "$SERVER_PID" 2>/dev/null || { cat "$LOG"; die "server exited during startup"; }
    sleep 0.5
done
[[ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "http://$REGISTRY/v2/" 2>/dev/null || true)" == "200" ]] \
    || { cat "$LOG"; die "server did not become ready on $REGISTRY"; }
ok "listening on $REGISTRY (pid $SERVER_PID)"

# -------------------------------------------------------------------- push ---

if [[ $DO_PUSH -eq 1 ]]; then
    command -v oras >/dev/null || die "oras is required to push (https://oras.land)"

    use_docker=0
    case "$SOURCE" in
        docker) use_docker=1 ;;
        remote) use_docker=0 ;;
        auto)
            if command -v docker >/dev/null && docker image inspect "$IMAGE" >/dev/null 2>&1; then
                use_docker=1
            fi ;;
        *) die "--source must be auto, docker or remote" ;;
    esac

    if [[ $use_docker -eq 1 ]]; then
        step "Exporting $IMAGE from the local docker daemon"
        docker image inspect "$IMAGE" >/dev/null 2>&1 || die "docker does not hold $IMAGE"
        if [[ $REFRESH_LAYOUT -eq 1 ]]; then rm -rf "$LAYOUT_DIR"; fi
        if [[ -f "$LAYOUT_DIR/index.json" ]]; then
            note "reusing cached layout $LAYOUT_DIR ($(du -sh "$LAYOUT_DIR" | cut -f1))"
        else
            rm -rf "$LAYOUT_DIR"; mkdir -p "$LAYOUT_DIR"
            t0=$(now)
            docker save "$IMAGE" | tar -x -C "$LAYOUT_DIR"
            note "exported in $(elapsed "$t0" "$(now)")s, $(du -sh "$LAYOUT_DIR" | cut -f1) on disk"
        fi
        # docker writes an OCI layout whose index carries the tag under
        # org.opencontainers.image.ref.name; oras copies straight from it.
        REF_NAME="$(jq -r '[.manifests[].annotations["org.opencontainers.image.ref.name"] // empty][0] // empty' "$LAYOUT_DIR/index.json")"
        [[ -n "$REF_NAME" ]] || die "no ref name in $LAYOUT_DIR/index.json"
        SRC_REF="$LAYOUT_DIR:$REF_NAME"
        PUSH_CMD=(oras cp --from-oci-layout "$SRC_REF" --to-plain-http)
        note "docker save exports layers uncompressed, so summ stores more bytes than a registry-to-registry copy would"
    else
        step "Copying $IMAGE from its upstream registry"
        PUSH_CMD=(oras cp "$IMAGE" --to-plain-http)
    fi

    # Appended after the branch rather than inside it, because bash 3.2 — which
    # is the bash macOS ships — cannot expand an empty array under `set -u`.
    if [[ $AUTH -eq 1 ]]; then
        PUSH_CMD+=(--to-username e2e --to-password "$WRITE_KEY")
    fi
    PUSH_CMD+=("$REGISTRY/$DEST_REPO:$DEST_TAG")

    step "Pushing to $REGISTRY/$DEST_REPO:$DEST_TAG"
    t0=$(now)
    "${PUSH_CMD[@]}" 2>&1 | sed 's/^/    /'
    PUSH_SECS=$(elapsed "$t0" "$(now)")
    ok "push finished in ${PUSH_SECS}s"
else
    step "Skipping push (--reuse-data)"
fi

# ----------------------------------------------------------- resolve image ---

step "Resolving $DEST_REPO:$DEST_TAG"

ACCEPT=(-H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json")

# HEAD first, the way containerd starts a pull: it must be a real single lookup,
# not a GET with the body thrown away.
head_out="$("${CURL[@]}" -I "${ACCEPT[@]}" -w '%{http_code} %{time_total}' -o "$RUN_DIR/head.txt" \
    "http://$REGISTRY/v2/$DEST_REPO/manifests/$DEST_TAG")"
head_code="${head_out%% *}"
[[ "$head_code" == "200" ]] || { cat "$RUN_DIR/head.txt"; die "HEAD manifest returned $head_code"; }
ok "HEAD manifests/$DEST_TAG → 200 in ${head_out##* }s"

"${CURL[@]}" "${ACCEPT[@]}" -D "$RUN_DIR/manifest.hdr" -o "$RUN_DIR/manifest.json" \
    "http://$REGISTRY/v2/$DEST_REPO/manifests/$DEST_TAG"

# The manifest must come back byte-exact: its digest is over the bytes as
# pushed, so re-hashing what we got is the whole check.
TOP_DIGEST="sha256:$(shasum -a 256 "$RUN_DIR/manifest.json" | awk '{print $1}')"
HDR_DIGEST="$(tr -d '\r' < "$RUN_DIR/manifest.hdr" | awk 'tolower($1)=="docker-content-digest:"{print $2}')"
if [[ -n "$HDR_DIGEST" && "$HDR_DIGEST" != "$TOP_DIGEST" ]]; then
    die "Docker-Content-Digest $HDR_DIGEST != sha256 of the body $TOP_DIGEST"
fi
ok "manifest returned byte-exact ($TOP_DIGEST)"

MEDIA="$(jq -r '.mediaType // ""' "$RUN_DIR/manifest.json")"
if [[ "$MEDIA" == *"index"* || "$MEDIA" == *"manifest.list"* ]]; then
    CHILD="$(jq -r '.manifests[0].digest' "$RUN_DIR/manifest.json")"
    CHILD_COUNT="$(jq -r '.manifests | length' "$RUN_DIR/manifest.json")"
    info "index with $CHILD_COUNT child manifest(s); exercising $CHILD"
    "${CURL[@]}" "${ACCEPT[@]}" -o "$RUN_DIR/image.json" \
        "http://$REGISTRY/v2/$DEST_REPO/manifests/$CHILD"
    got="sha256:$(shasum -a 256 "$RUN_DIR/image.json" | awk '{print $1}')"
    [[ "$got" == "$CHILD" ]] || die "child manifest digest mismatch: $got != $CHILD"
    ok "child manifest byte-exact"
else
    cp "$RUN_DIR/manifest.json" "$RUN_DIR/image.json"
fi

# One blob list — config first, the way a client needs it, then the layers.
# Foreign layers (a `urls` descriptor) are skipped: the registry deliberately
# does not hold them.
jq -r '[.config] + [.layers[] | select((.urls // []) | length == 0)]
       | .[] | "\(.digest) \(.size)"' "$RUN_DIR/image.json" > "$RUN_DIR/blobs.txt"

BLOB_COUNT=$(wc -l < "$RUN_DIR/blobs.txt" | tr -d ' ')
TOTAL_BYTES=$(awk '{s+=$2} END{print s+0}' "$RUN_DIR/blobs.txt")
LARGEST=$(awk '{if ($2>m) m=$2} END{print m+0}' "$RUN_DIR/blobs.txt")
info "$BLOB_COUNT blobs, $(human "$TOTAL_BYTES") total, largest $(human "$LARGEST")"
if [[ -n "${PUSH_SECS:-}" ]]; then
    info "push throughput ≈ $(rate "$TOTAL_BYTES" "$PUSH_SECS")"
fi

# -------------------------------------------------------------- range checks ---

step "Range requests"
FIRST_BIG=$(sort -k2 -n -r "$RUN_DIR/blobs.txt" | head -1 | awk '{print $1}')
FIRST_BIG_SIZE=$(sort -k2 -n -r "$RUN_DIR/blobs.txt" | head -1 | awk '{print $2}')
BLOB_URL="http://$REGISTRY/v2/$DEST_REPO/blobs/$FIRST_BIG"

code=$("${CURL[@]}" -o "$RUN_DIR/range.bin" -w '%{http_code}' -r 0-1023 "$BLOB_URL")
size=$(wc -c < "$RUN_DIR/range.bin" | tr -d ' ')
[[ "$code" == "206" && "$size" == "1024" ]] || die "bytes=0-1023 gave $code with $size bytes"
ok "bytes=0-1023 → 206, 1024 bytes"

# containerd 2.1+ opens `bytes=N-`, reads ~8 MiB and drops the connection. head
# closing the pipe is exactly that abort; the check is that the server survives
# it and answers the next request.
OFFSET=$(( FIRST_BIG_SIZE > 33554432 ? 33554432 : 0 ))
set +o pipefail
got=$("${CURL[@]}" -r "$OFFSET-" "$BLOB_URL" 2>/dev/null | head -c 8388608 | wc -c | tr -d ' ')
set -o pipefail
[[ "$got" == "8388608" ]] || die "open-ended bytes=$OFFSET- delivered $got bytes before the abort"
ok "open-ended bytes=$OFFSET- aborted after 8 MiB"

code=$("${CURL[@]}" -o /dev/null -w '%{http_code}' -I "$BLOB_URL")
[[ "$code" == "200" ]] || die "server unhealthy after an aborted read (HEAD blob → $code)"
ok "server healthy after the abort"

# --------------------------------------------------------------- pull stress ---

step "Pull stress: $THREADS concurrent pullers × $ROUNDS round(s)"

cat > "$RUN_DIR/pull-worker.sh" <<'WORKER'
#!/usr/bin/env bash
# One full pull: manifest, then every blob, digest-verified unless told not to.
set -uo pipefail
id="$1"
out="$RUN_DIR/stats/$id"
started=$(python3 -c 'import time; print(time.time())')
fail=0

curl_args=(curl -sS --fail-with-body)
[[ -n "$AUTHKEY" ]] && curl_args+=(-u "e2e:$AUTHKEY")

# Manifest first, as a client does, so every round exercises the metadata path.
if ! "${curl_args[@]}" -o /dev/null \
        -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json" \
        "http://$REGISTRY/v2/$REPO/manifests/$TAG" >"$out.err" 2>&1; then
    echo "worker $id: manifest fetch failed" >> "$out.err"
    fail=1
fi

while read -r digest size; do
    [[ -n "$digest" ]] || continue
    url="http://$REGISTRY/v2/$REPO/blobs/$digest"
    if [[ "$VERIFY" == "1" ]]; then
        got="sha256:$("${curl_args[@]}" "$url" 2>>"$out.err" | shasum -a 256 | awk '{print $1}')"
        if [[ "$got" != "$digest" ]]; then
            echo "worker $id: $digest came back as $got" >> "$out.err"
            fail=1
        fi
    else
        n=$("${curl_args[@]}" -o /dev/null -w '%{size_download}' "$url" 2>>"$out.err")
        if [[ "$n" != "$size" ]]; then
            echo "worker $id: $digest delivered $n of $size bytes" >> "$out.err"
            fail=1
        fi
    fi
done < "$BLOBS"

finished=$(python3 -c 'import time; print(time.time())')
awk -v a="$started" -v b="$finished" -v f="$fail" 'BEGIN{printf "%.3f %d\n", b-a, f}' > "$out.stat"
WORKER
chmod +x "$RUN_DIR/pull-worker.sh"

rm -rf "$RUN_DIR/stats"; mkdir -p "$RUN_DIR/stats"
export RUN_DIR REGISTRY
export REPO="$DEST_REPO" TAG="$DEST_TAG" BLOBS="$RUN_DIR/blobs.txt"
export VERIFY="$DO_VERIFY"
export AUTHKEY="$([[ $AUTH -eq 1 ]] && echo "$WRITE_KEY" || echo "")"

JOBS=$(( THREADS * ROUNDS ))
info "$JOBS pulls of $(human "$TOTAL_BYTES") each — $(human $(( TOTAL_BYTES * JOBS ))) in total"
t0=$(now)
seq 1 "$JOBS" | xargs -P "$THREADS" -n 1 "$RUN_DIR/pull-worker.sh" || true
STRESS_SECS=$(elapsed "$t0" "$(now)")

failed=0; completed=0
for f in "$RUN_DIR"/stats/*.stat; do
    [[ -f "$f" ]] || continue
    completed=$(( completed + 1 ))
    read -r secs code < "$f"
    [[ "$code" == "0" ]] || failed=$(( failed + 1 ))
    note "pull $(basename "$f" .stat): ${secs}s $([[ "$code" == "0" ]] && echo verified || echo FAILED)"
done

missing=$(( JOBS - completed ))
MOVED=$(( TOTAL_BYTES * completed ))
info "wall clock ${STRESS_SECS}s, aggregate $(rate "$MOVED" "$STRESS_SECS")"

if [[ $failed -gt 0 || $missing -gt 0 ]]; then
    for f in "$RUN_DIR"/stats/*.err; do
        if [[ -s "$f" ]]; then sed 's/^/    /' "$f"; fi
    done
    die "$failed pull(s) failed, $missing never finished"
fi
ok "$completed/$JOBS pulls completed$([[ $DO_VERIFY -eq 1 ]] && echo ', every blob digest verified')"

# ----------------------------------------------------------------- discovery ---

step "Discovery API"
"${CURL[@]}" "http://$REGISTRY/api/v1/repositories" -o "$RUN_DIR/repos.json"
if jq -e --arg r "$DEST_REPO" '.repositories[]? | select(.name == $r)' "$RUN_DIR/repos.json" >/dev/null; then
    ok "listed by /api/v1/repositories"
else
    warn "$DEST_REPO not in the first page of /api/v1/repositories"
fi

# The per-repository resource is where the counts and the size live. `complete`
# says whether a count is exact or a floor at COUNT_CEILING, so print it.
"${CURL[@]}" "http://$REGISTRY/api/v1/repositories/$DEST_REPO" -o "$RUN_DIR/repo.json"
jq -r '"    tags \(.tags.count)\(if .tags.complete then "" else "+" end), " +
       "manifests \(.manifests.count)\(if .manifests.complete then "" else "+" end), " +
       "blobs \(.blobs.count)\(if .blobs.complete then "" else "+" end), " +
       "\(.size_bytes) bytes"' "$RUN_DIR/repo.json"

api_bytes=$(jq -r '.size_bytes' "$RUN_DIR/repo.json")
# A repository's size counts its blobs, not the archived copy of its manifests,
# so it is expected to sit just under the total the manifest declares.
if [[ "$api_bytes" -ge "$TOTAL_BYTES" ]]; then
    ok "reported size covers every layer"
else
    note "reported size is $(human "$api_bytes") against $(human "$TOTAL_BYTES") of declared blobs"
fi

"${CURL[@]}" "http://$REGISTRY/api/v1/tags/$DEST_REPO" -o "$RUN_DIR/tags.json"
jq -e --arg t "$DEST_TAG" '.tags[]? | select(.name == $t or . == $t)' "$RUN_DIR/tags.json" >/dev/null \
    && ok "tag $DEST_TAG listed by /api/v1/tags" \
    || warn "tag $DEST_TAG missing from /api/v1/tags"

# ------------------------------------------------------------------ restart ---

step "Restart durability"
kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true
"$BIN" "${SERVE_ARGS[@]}" >>"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
    [[ "$("${CURL[@]}" -o /dev/null -w '%{http_code}' "http://$REGISTRY/v2/" 2>/dev/null || true)" == "200" ]] && break
    sleep 0.5
done
after="$("${CURL[@]}" "${ACCEPT[@]}" "http://$REGISTRY/v2/$DEST_REPO/manifests/$DEST_TAG" | shasum -a 256 | awk '{print $1}')"
[[ "sha256:$after" == "$TOP_DIGEST" ]] || die "manifest changed across a restart: sha256:$after != $TOP_DIGEST"
ok "manifest survived a restart byte-exact"

DISK=$(du -sh "$DATA_DIR" 2>/dev/null | cut -f1)
step "Done"
info "image     $IMAGE ($BLOB_COUNT blobs, $(human "$TOTAL_BYTES"), largest $(human "$LARGEST"))"
[[ -n "${PUSH_SECS:-}" ]] && info "push      ${PUSH_SECS}s ≈ $(rate "$TOTAL_BYTES" "$PUSH_SECS")"
info "pulls     $completed × $(human "$TOTAL_BYTES") in ${STRESS_SECS}s ≈ $(rate "$MOVED" "$STRESS_SECS")"
info "on disk   $DISK in $DATA_DIR"
printf '\n%sregistry survived a %s image end to end.%s\n' "$GRN" "$(human "$TOTAL_BYTES")" "$N"
