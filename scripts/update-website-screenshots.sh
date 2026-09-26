#!/usr/bin/env bash
#
# Refreshes the website's front-page screenshots from a live instance.
#
# Spins up the e2e stack (server + one real privileged build worker), builds a
# few small AUR packages in it, screenshots the dashboard, a package page, a
# build log and a narrow mobile viewport with a headless browser, and copies
# the results over docs/static/img/. Run it whenever the UI has visibly
# changed and the front page still shows the old one.
#
#   ./scripts/update-website-screenshots.sh
#   ./scripts/update-website-screenshots.sh --keep     # leave the stack running
#   ./scripts/update-website-screenshots.sh --check    # only show what would run
#
# Env overrides: PORT (default 8080), PACKAGES (default "hello neofetch
# downgrade"), BUILD_TIMEOUT (default 900), CHROME (path to a chrome/chromium
# binary), REUSE=1 (reuse a running stack instead of starting fresh).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
COMPOSE_FILE="$PROJECT_DIR/compose/docker-compose.e2e.yaml"
SHOTS_DIR="$(mktemp -d -t aurcache-shots-XXXXXX)"
CLI_BIN="$PROJECT_DIR/backend/target/debug/aurcli"

PORT="${PORT:-8080}"
PACKAGES="${PACKAGES:-hello neofetch downgrade}"
BUILD_TIMEOUT="${BUILD_TIMEOUT:-900}"
# The package the package-page and build-log screenshots are taken from: the
# first one built, so its build is always number 1.
SHOT_PKG="${SHOT_PKG:-}"
KEEP=0
CHECK=0
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1; shift ;;
        --check) CHECK=1; shift ;;
        --port) PORT="${2:?--port needs a value}"; shift 2 ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -z "$SHOT_PKG" ] && SHOT_PKG="${PACKAGES%% *}"

export AURCACHE_URL="http://localhost:$PORT/api"

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
    for candidate in google-chrome-stable google-chrome chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then CHROME="$candidate"; break; fi
    done
fi

log() { echo "[$(date '+%H:%M:%S')] $*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }
dc() { docker compose -f "$COMPOSE_FILE" "$@"; }

cleanup() {
    rm -rf "$SHOTS_DIR"
    if [ "$CHECK" = 1 ]; then
        return
    fi
    if [ "$KEEP" = 1 ]; then
        log "Leaving the stack running (port $PORT); tear down with:"
        log "    docker compose -f '$COMPOSE_FILE' down -v --remove-orphans"
        return
    fi
    log "Tearing down"
    dc down -v --remove-orphans -t 10 2>/dev/null || true
}
trap cleanup EXIT

preflight() {
    local missing=0
    for tool in docker jq curl cargo; do
        command -v "$tool" >/dev/null 2>&1 || { echo "ERROR: '$tool' is required but not installed" >&2; missing=1; }
    done
    docker compose version >/dev/null 2>&1 || { echo "ERROR: 'docker compose' (v2) is required" >&2; missing=1; }
    docker info >/dev/null 2>&1 || { echo "ERROR: cannot talk to the Docker daemon" >&2; missing=1; }
    [ -n "$CHROME" ] || { echo "ERROR: no chrome/chromium found; set CHROME=/path/to/browser" >&2; missing=1; }
    [ "$missing" -eq 0 ] || exit 1
}

port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

bring_up() {
    if [ "${REUSE:-0}" = 1 ] && [ -n "$(dc ps -q aurcache 2>/dev/null)" ]; then
        log "Reusing the running stack (REUSE=1)"
        return
    fi
    log "Building images (server + worker)"
    dc build
    log "Starting from a fresh instance"
    dc down -v --remove-orphans -t 10 2>/dev/null || true
    AURCACHE_PORT="$PORT" AURCACHE_MIRROR_PORT=$((PORT + 1)) AURCACHE_WORKER_PORT=$((PORT + 3)) dc up -d
}

wait_for_api() {
    log "Waiting for the API on port $PORT"
    for _ in $(seq 1 60); do
        curl -fsS "http://localhost:$PORT/api/packages/list?limit=1" -o /dev/null 2>/dev/null && return 0
        sleep 2
    done
    fail "API did not come up on port $PORT"
}

wait_for_worker() {
    log "Waiting for an approved worker"
    for _ in $(seq 1 60); do
        local approved
        approved=$(curl -fsS "http://localhost:$PORT/api/workers" 2>/dev/null \
            | jq -r '[.[] | select(.status == "approved")] | length' 2>/dev/null || echo 0)
        if [ "${approved:-0}" -ge 1 ]; then
            log "Worker approved"
            return 0
        fi
        sleep 2
    done
    fail "no worker approved in time"
}

build_packages() {
    log "Building the CLI"
    ( cd "$PROJECT_DIR/backend" && cargo build -q -p aurcache-cli )
    local pkg
    # shellcheck disable=SC2086
    for pkg in $PACKAGES; do
        log "Adding package: $pkg"
        "$CLI_BIN" pkg add "$pkg" --platform x86_64 \
            --wait --wait-timeout "$BUILD_TIMEOUT" --fail-on-requeue \
            || fail "package $pkg failed to build"
    done
}

shot() {
    local url="$1" out="$2" width="$3" height="$4"
    "$CHROME" --headless --no-sandbox --disable-gpu --hide-scrollbars \
        --window-size="$width,$height" --virtual-time-budget=15000 \
        --screenshot="$out" "$url" 2>/dev/null | tail -n 1
}

take_screenshots() {
    local base="http://localhost:$PORT"
    log "Screenshotting the dashboard"
    shot "$base/" "$SHOTS_DIR/screenshot1.png" 2560 1440
    log "Screenshotting the $SHOT_PKG package page"
    shot "$base/package/$SHOT_PKG" "$SHOTS_DIR/screenshot2.png" 2560 1440
    log "Screenshotting the $SHOT_PKG build log"
    shot "$base/package/$SHOT_PKG/build/1" "$SHOTS_DIR/screenshot3.png" 2560 1440
    log "Screenshotting a mobile viewport"
    shot "$base/" "$SHOTS_DIR/screenshot_mobile1.png" 390 844
}

install_shots() {
    log "Updating docs/static/img/"
    cp "$SHOTS_DIR/screenshot1.png" "$PROJECT_DIR/docs/static/img/screenshot1.png"
    cp "$SHOTS_DIR/screenshot2.png" "$PROJECT_DIR/docs/static/img/screenshot2.png"
    cp "$SHOTS_DIR/screenshot3.png" "$PROJECT_DIR/docs/static/img/screenshot3.png"
    cp "$SHOTS_DIR/screenshot_mobile1.png" "$PROJECT_DIR/docs/static/img/screenshot_mobile1.png"
    ( cd "$PROJECT_DIR" && git status --short docs/static/img/ )
    log "Rebuild the site with: cd docs && yarn build"
}

main() {
    preflight
    if port_busy "$PORT" && [ "${REUSE:-0}" != 1 ]; then
        fail "port $PORT is already in use — stop the other server first or pass --port"
    fi
    if [ "$CHECK" = 1 ]; then
        log "CHECK: would bring up $COMPOSE_FILE on port $PORT, build: $PACKAGES,"
        log "CHECK: screenshot $SHOT_PKG + mobile, and update docs/static/img/"
        exit 0
    fi
    bring_up
    wait_for_api
    wait_for_worker
    build_packages
    take_screenshots
    install_shots
    log "Done"
}

main
