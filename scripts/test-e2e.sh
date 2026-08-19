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

dump_logs_on_failure() {
    dc logs -t > "$LOG_FILE" 2>&1 || true
    echo "--- aurcache service logs (tail; full multi-service logs in $LOG_FILE) ---"
    dc logs aurcache 2>&1 | tail -n 150
    echo "--- builder (worker) logs (tail) ---"
    dc logs builder 2>&1 | tail -n 150
}

cleanup() {
    local exit_code=$?
    if [ "$exit_code" -ne 0 ] && [ "${CLEANUP:-1}" != "1" ]; then
        echo "=== Test failed (exit $exit_code): leaving containers up for debugging ==="
        echo "    Logs: dc logs   (compose file: $COMPOSE_FILE)"
        echo "    Full logs saved to: $LOG_FILE"
        echo "    Clean up with: docker compose -f '$COMPOSE_FILE' down -v --remove-orphans"
        return
    fi
    if [ "${CLEANUP:-1}" = "1" ]; then
        log "=== Cleaning up ==="
        # Named volumes make cleanup trivial and root-owned-file-proof.
        dc down -v --remove-orphans -t 10 2>/dev/null || true
    else
        log "=== Skipping cleanup (CLEANUP=0) ==="
    fi
}

wait_for_service() {
    log "=== Waiting for AURCache API to be ready ==="
    for i in $(seq 1 60); do
        if aurcache_cli health > /dev/null 2>&1; then
            echo "    AURCache is ready"
            return 0
        fi
        [ "$i" -eq 60 ] && return 1
        sleep 2
    done
}

wait_for_worker() {
    log "=== Waiting for a worker to enroll and be approved ==="
    # OAuth is disabled in this setup, so /api/workers is readable without auth.
    for i in $(seq 1 60); do
        local approved
        approved=$(curl -fsS "http://localhost:$AURCACHE_PORT/api/workers" 2>/dev/null \
            | jq -r '[.[] | select(.status == "approved")] | length' 2>/dev/null || echo 0)
        if [ "${approved:-0}" -ge 1 ]; then
            echo "    Worker approved and connected"
            return 0
        fi
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
    if ! aurcache_cli pkg add aur "$PACKAGE" --platform x86_64; then
        echo "ERROR: Package request failed"
        dump_logs_on_failure
        exit 1
    fi

    log "=== Waiting for build to complete (timeout: ${BUILD_TIMEOUT}s) ==="
    local start_time
    start_time=$(date +%s)
    while true; do
        local elapsed
        elapsed=$(($(date +%s) - start_time))
        if [ "$elapsed" -gt "$BUILD_TIMEOUT" ]; then
            log "ERROR: Build timed out after ${BUILD_TIMEOUT}s"
            dump_logs_on_failure
            exit 1
        fi

        local status
        status=$(aurcache_cli --format json pkg list --limit 100 \
            | jq -r ".[] | select(.name == \"$PACKAGE\") | .status" 2>/dev/null || echo "")

        log "    Build status: ${status:-<none>} (elapsed: ${elapsed}s)"
        case "$status" in
            1)  log "    Build completed successfully"; break ;;
            2)  log "ERROR: Build failed"; dump_logs_on_failure; exit 1 ;;
            *)  sleep 5 ;;
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
            cat >> /etc/pacman.conf << EOF
[repo]
SigLevel = Optional TrustAll
Server = http://aurcache:8081/$arch
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

build_and_start
wait_for_service   || { dump_logs_on_failure; exit 1; }
wait_for_worker    || { dump_logs_on_failure; exit 1; }
request_package
validate
