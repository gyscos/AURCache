#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for the remote-worker architecture.
#
# Brings up the AURCache server + one real privileged build worker (see
# docker-compose.e2e.yaml), then: requests a package, waits for the worker to
# enroll/approve, waits for the build to finish, and finally installs the built
# package from the repo in a throwaway container. Single mode only.
#
#   ./scripts/test-e2e.sh <package> [port] [timeout]

: "${1?Usage: $0 <package> [port] [timeout]}"
PACKAGE="$1"
export AURCACHE_PORT="${2:-8080}"
export AURCACHE_MIRROR_PORT=$((AURCACHE_PORT + 1))
export AURCACHE_WORKER_PORT=$((AURCACHE_PORT + 3))
BUILD_TIMEOUT="${3:-600}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CLI_BIN="$PROJECT_DIR/backend/target/debug/aurcache-cli"

# The human-facing API is plain HTTP now (worker mTLS lives on its own port), so
# the CLI needs no TLS to talk to it.
export AURCACHE_URL="http://localhost:$AURCACHE_PORT/api"
export AURCACHE_TOKEN="${AURCACHE_TOKEN:-}"

COMPOSE_FILE="$PROJECT_DIR/docker-compose.e2e.yaml"
LOG_FILE="$(mktemp -t aurcache-e2e-XXXXXX.log)"

# =============================================================================
# Helpers
# =============================================================================

log() { echo "[$(date '+%H:%M:%S')] $*"; }

dc() { docker compose -f "$COMPOSE_FILE" "$@"; }

aurcache_cli() { "$CLI_BIN" "$@"; }

# Fail early with an actionable message instead of an obscure error 3 minutes in.
preflight() {
    local missing=0
    for tool in docker jq curl cargo; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not installed"; missing=1; }
    done
    docker compose version >/dev/null 2>&1 || { echo "ERROR: 'docker compose' (v2) is required"; missing=1; }
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon"; missing=1; }
    [ "$missing" -eq 0 ] || exit 1
}

# Build status codes -> names, so progress reads as "enqueued" not "3".
status_name() {
    case "${1:-}" in
        0) echo "active" ;;
        1) echo "success" ;;
        2) echo "failed" ;;
        3) echo "enqueued" ;;
        4) echo "waiting-for-deps" ;;
        "") echo "<none>" ;;
        *) echo "unknown($1)" ;;
    esac
}

# True if a service container has exited. A dead container will never satisfy
# any wait loop, so polling it for the full timeout just delays the diagnosis.
service_exited() {
    local svc="$1" cid
    cid=$(dc ps -aq "$svc" 2>/dev/null | head -1)
    [ -n "$cid" ] || return 1
    [ "$(docker inspect -f '{{.State.Status}}' "$cid" 2>/dev/null)" = "exited" ]
}

# Abort as soon as either container dies, naming the culprit and its last words.
assert_services_alive() {
    local svc
    for svc in aurcache builder; do
        if service_exited "$svc"; then
            log "ERROR: container '$svc' exited unexpectedly"
            echo "--- last 40 lines from '$svc' ---"
            dc logs "$svc" 2>&1 | filter_noise | tail -n 40
            dump_logs_on_failure
            exit 1
        fi
    done
}

# The services log at debug, and hyper/h2/rustls emit a frame line per I/O op,
# which buries the handful of lines that explain a failure. Drop that unless
# VERBOSE=1.
filter_noise() {
    if [ "${VERBOSE:-0}" = "1" ]; then
        cat
    else
        grep -viE 'h2::|hyper_util::|hyper::proto|rustls::|framed_read|framed_write|Ping \{|tower::buffer' || true
    fi
}

# AURCache's own build log is what actually explains a failed build; container
# logs mostly show the plumbing around it.
dump_build_log() {
    local build_id="$1"
    [ -n "$build_id" ] || return 0
    echo "--- AURCache build log (build $build_id) ---"
    aurcache_cli builds output "$build_id" 2>&1 | tail -n 100 || echo "    (build log unavailable)"
}

dump_logs_on_failure() {
    dc logs -t > "$LOG_FILE" 2>&1 || true
    dump_build_log "${CURRENT_BUILD_ID:-}"
    echo "--- aurcache service logs (tail; full unfiltered logs in $LOG_FILE) ---"
    dc logs aurcache 2>&1 | filter_noise | tail -n 60
    echo "--- builder (worker) logs (tail) ---"
    dc logs builder 2>&1 | filter_noise | tail -n 60
    echo "--- hint: re-run with VERBOSE=1 for unfiltered logs, CLEANUP=0 to keep containers ---"
}

cleanup() {
    local exit_code=$?
    # Cleanup policy:
    #   CLEANUP=1  -> always tear down (even on failure; e.g. CI)
    #   CLEANUP=0  -> never tear down
    #   CLEANUP unset (default): tear down on success, PRESERVE on failure so a
    #                            failed run can be debugged.
    local do_cleanup
    if [ "${CLEANUP:-}" = "1" ]; then
        do_cleanup=1
    elif [ "${CLEANUP:-}" = "0" ]; then
        do_cleanup=0
    elif [ "$exit_code" -eq 0 ]; then
        do_cleanup=1
    else
        do_cleanup=0
    fi

    if [ "$do_cleanup" = "0" ]; then
        echo "=== Leaving containers up (exit $exit_code) ==="
        echo "    Logs: dc logs   (compose file: $COMPOSE_FILE)"
        echo "    Full logs saved to: $LOG_FILE"
        echo "    Clean up with: docker compose -f '$COMPOSE_FILE' down -v --remove-orphans"
        return
    fi

    log "=== Cleaning up ==="
    # Named volumes make cleanup trivial and root-owned-file-proof.
    dc down -v --remove-orphans -t 10 2>/dev/null || true
}

wait_for_service() {
    log "=== Waiting for AURCache API to be ready ==="
    local started; started=$(date +%s)
    for i in $(seq 1 60); do
        if aurcache_cli health > /dev/null 2>&1; then
            echo "    AURCache is ready (after $(($(date +%s) - started))s)"
            return 0
        fi
        # A crashed server (bad image, port clash, migration failure) will never
        # become ready; say so now instead of after the full 120s.
        assert_services_alive
        [ "$i" -eq 60 ] && { log "ERROR: AURCache API not ready after 120s"; return 1; }
        sleep 2
    done
}

wait_for_worker() {
    log "=== Waiting for a worker to enroll and be approved ==="
    local started; started=$(date +%s)
    # OAuth is disabled in this setup, so /api/workers is readable without auth.
    for i in $(seq 1 60); do
        local approved
        approved=$(curl -fsS "http://localhost:$AURCACHE_PORT/api/workers" 2>/dev/null \
            | jq -r '[.[] | select(.status == "approved")] | length' 2>/dev/null || echo 0)
        if [ "${approved:-0}" -ge 1 ]; then
            echo "    Worker approved and connected (after $(($(date +%s) - started))s)"
            return 0
        fi
        assert_services_alive
        [ "$i" -eq 60 ] && { log "ERROR: no worker approved in time"; return 1; }
        sleep 2
    done
}

# =============================================================================
# Steps
# =============================================================================

build_and_start() {
    log "=== Building AURCache CLI ==="
    ( cd "$PROJECT_DIR/backend" && cargo build -q -p aurcache-cli )

    log "=== Building images (server + worker) ==="
    dc build

    log "=== Starting services ==="
    dc up -d
}

request_package() {
    log "=== Adding package: $PACKAGE ==="
    # `pkg add` takes package names positionally and auto-detects git URLs; the
    # old `add aur <name>` / `add git <url>` subcommands are gone. Passing `aur`
    # here made it the *first package name*, so the run died trying to add a
    # nonexistent AUR package called "aur".
    if ! aurcache_cli pkg add "$PACKAGE" --platform x86_64; then
        echo "ERROR: Package request failed"
        dump_logs_on_failure
        exit 1
    fi

    log "=== Waiting for build to complete (timeout: ${BUILD_TIMEOUT}s) ==="
    local start_time prev_status="" reached_active=0
    start_time=$(date +%s)
    while true; do
        local elapsed
        elapsed=$(($(date +%s) - start_time))
        if [ "$elapsed" -gt "$BUILD_TIMEOUT" ]; then
            log "ERROR: Build timed out after ${BUILD_TIMEOUT}s (last status: $(status_name "$prev_status"))"
            dump_logs_on_failure
            exit 1
        fi

        local status
        status=$(aurcache_cli --format json pkg list --limit 100 \
            | jq -r ".[] | select(.name == \"$PACKAGE\") | .status" 2>/dev/null || echo "")

        # Track the build id so failures can dump AURCache's own build log.
        CURRENT_BUILD_ID=$(aurcache_cli --format json builds list --limit 20 2>/dev/null \
            | jq -r "[.[] | select(.pkg_name == \"$PACKAGE\")] | max_by(.id) | .id // empty" 2>/dev/null || echo "")

        # Only speak when something changes: this loop polls every 5s and used
        # to print an identical line each time, burying real events.
        if [ "$status" != "$prev_status" ]; then
            log "    Build status: $(status_name "$status") (elapsed: ${elapsed}s)"
            [ "$status" = "0" ] && reached_active=1

            # A build that goes back to enqueued after being active was requeued,
            # which means the worker's completion was refused. Without this the
            # run just loops build->reject->rebuild until the timeout, showing
            # nothing but a steady "enqueued".
            if [ "$reached_active" = "1" ] && [ "$status" = "3" ]; then
                log "ERROR: build was requeued after running - the server rejected the worker's completion"
                log "       (this loops forever; failing now rather than at the ${BUILD_TIMEOUT}s timeout)"
                dump_logs_on_failure
                exit 1
            fi
            prev_status="$status"
        fi

        case "$status" in
            1)  log "    Build completed successfully (in ${elapsed}s)"; break ;;
            2)  log "ERROR: Build failed"; dump_logs_on_failure; exit 1 ;;
            *)  assert_services_alive; sleep 5 ;;
        esac
    done
}

validate() {
    log "=== Validating built package by installing it from the repo ==="
    # Derive the actual compose network name from the running server container so
    # this doesn't depend on the compose project name.
    local net
    net=$(dc ps -q aurcache | head -1 \
        | xargs docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}}{{end}}')
    log "    Using network: $net"
    docker run --rm \
        --network "$net" \
        archlinux:latest \
        sh -e -c '
            # `\$arch` must reach pacman.conf literally for pacman to expand it
            # to the repo`s arch subdirectory. The heredoc delimiter is unquoted,
            # so an unescaped $arch would be eaten by this shell and leave
            # `Server = http://aurcache:8081/`, which has no repo.db.
            cat >> /etc/pacman.conf << EOF
[repo]
SigLevel = Optional TrustAll
Server = http://aurcache:8081/\$arch
EOF
            (
                pacman-key --init
                pacman-key --populate archlinux
                pacman -Syq archlinux-keyring --noconfirm
                pacman -Suq --noconfirm
            ) >/dev/null 2>&1
            echo "Installing '"$PACKAGE"'"
            pacman -S --noconfirm '"$PACKAGE"'
            pacman -Qi '"$PACKAGE"'
        '
    log "=== End-to-end test complete ==="
}

# =============================================================================
# Main
# =============================================================================

trap cleanup EXIT

RUN_STARTED=$(date +%s)

preflight
build_and_start
wait_for_service   || { dump_logs_on_failure; exit 1; }
wait_for_worker    || { dump_logs_on_failure; exit 1; }
request_package
validate

log "=== Total runtime: $(($(date +%s) - RUN_STARTED))s ==="
