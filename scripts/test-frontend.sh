#!/usr/bin/env bash
#
# Renders the Rust frontend in a real browser and asserts each route mounts.
#
# The unit and SSR tests render components directly, so they cannot see whether
# the page actually boots. Every defect of that kind this frontend has had was
# found by opening it: a blank page, a status badge with no background, a table
# that clipped its last column. This is that pass, scripted.
#
# The check is "did the app mount and render this screen", not a pixel
# comparison. Screenshots are captured with --shots for a human to look at;
# they are not asserted on.
#
# Usage:
#   scripts/test-frontend.sh                # smoke check, no network needed
#   scripts/test-frontend.sh --shots out/   # also write screenshots there
#   scripts/test-frontend.sh --online       # include routes that fetch from the AUR
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SEED="$PROJECT_DIR/scripts/fixtures/frontend-seed.sql"

# The API port is compiled in (aurcache_types::ports::AURCACHE_HTTP_PORT), so it
# cannot be moved out of the way.
API_PORT=8080
UI_PORT=8099

SHOTS=""
ONLINE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --shots) SHOTS="${2:?--shots needs a directory}"; shift 2 ;;
        --online) ONLINE=1; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

CHROME="${CHROME:-}"
if [ -z "$CHROME" ]; then
    for candidate in google-chrome-stable google-chrome chromium chromium-browser; do
        if command -v "$candidate" >/dev/null 2>&1; then CHROME="$candidate"; break; fi
    done
fi

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- preflight -------------------------------------------------------------
# All of this is cheaper to check now than to debug from a blank screenshot.

[ -n "$CHROME" ] || fail "no chrome/chromium found; set CHROME=/path/to/browser"
command -v wasm-bindgen >/dev/null 2>&1 || fail "wasm-bindgen not installed (cargo install wasm-bindgen-cli)"
command -v sqlite3 >/dev/null 2>&1 || fail "sqlite3 not installed"
rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown \
    || fail "rust target wasm32-unknown-unknown not installed"

# A server left running from an earlier run answers on these ports and the whole
# suite then silently tests *that* build. This has happened; refuse instead.
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
port_busy "$API_PORT" && fail "port $API_PORT is already in use — stop the other server first"
port_busy "$UI_PORT"  && fail "port $UI_PORT is already in use — stop the other server first"

WORKDIR="$(mktemp -d)"
BACKEND_PID=""
UI_PID=""
cleanup() {
    [ -n "$UI_PID" ] && kill "$UI_PID" 2>/dev/null || true
    [ -n "$BACKEND_PID" ] && kill "$BACKEND_PID" 2>/dev/null || true
    # Wait for them to release the ports, or an immediate rerun trips the
    # busy-port preflight.
    [ -n "$UI_PID" ] && wait "$UI_PID" 2>/dev/null || true
    [ -n "$BACKEND_PID" ] && wait "$BACKEND_PID" 2>/dev/null || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

# --- build -----------------------------------------------------------------

echo "==> building the frontend"
( cd "$PROJECT_DIR/frontend-rs" \
    && cargo build --release --quiet --target wasm32-unknown-unknown \
    && wasm-bindgen --target web --out-dir dist --no-typescript \
         target/wasm32-unknown-unknown/release/aurcache-frontend.wasm \
    && cp index.html dist/index.html )

echo "==> building the server"
( cd "$PROJECT_DIR/backend" && cargo build --quiet -p aurcache )

# --- run -------------------------------------------------------------------

echo "==> starting the server"
# A long interval keeps the scheduler's second pass outside this run. Its first
# pass still happens at boot, which is why the fixture is applied afterwards:
# it sees an empty table and rewrites nothing.
# `exec` so the subshell is *replaced* by the server: without it $! is the
# subshell's pid, killing that leaves the server running, and the next run
# aborts on a busy port.
( cd "$WORKDIR" && exec env VERSION_CHECK_INTERVAL=86400 \
    "$PROJECT_DIR/backend/target/debug/aurcache" > "$WORKDIR/server.log" 2>&1 ) &
BACKEND_PID=$!

for _ in $(seq 1 120); do
    curl -sf "http://localhost:$API_PORT/api/packages/list?limit=1" -o /dev/null && break
    sleep 1
done
curl -sf "http://localhost:$API_PORT/api/packages/list?limit=1" -o /dev/null \
    || { tail -20 "$WORKDIR/server.log"; fail "server did not come up"; }

echo "==> seeding fixture data"
sqlite3 "$WORKDIR/db/db.sqlite" < "$SEED"

echo "==> serving the frontend"
( cd "$PROJECT_DIR/frontend-rs" \
    && exec python3 serve.py "http://localhost:$API_PORT" "$UI_PORT" > "$WORKDIR/ui.log" 2>&1 ) &
UI_PID=$!
for _ in $(seq 1 60); do
    curl -sf "http://localhost:$UI_PORT/" -o /dev/null && break
    sleep 0.5
done
curl -sf "http://localhost:$UI_PORT/" -o /dev/null || fail "ui server did not come up"

# --- checks ----------------------------------------------------------------
#
# Each entry is  route|marker|description.
#
# `marker` is something only that screen renders, so the check proves the right
# screen mounted rather than merely that *a* page loaded.
#
# Routes two or more segments deep are the important ones and must stay in this
# list. The relative-asset-path bug that shipped blank pages left `/` and
# `/builds` working — at one segment deep a relative specifier still resolves
# correctly — and broke only `/build/1` and below. A list of shallow routes
# would have passed straight through it.
ROUTES=(
    "/|Needs a charting story|dashboard"
    "/builds|Duration|builds list"
    "/build/1|Build #1|build log (depth 2)"
    "/packages|Upstream|packages list"
    "/packages|—|missing version shows a placeholder"
    "/package/hello|Dependencies|package detail (depth 2)"
    "/package/hello|In repo|package detail shows what is in the repo"
    "/package/hello/builds|Platform|per-package build history (depth 3)"
    "/package/yay|blocking|a blocked package names what is holding it back"
    "/package/yay|too old|a dependency built to an unsatisfying version"
    "/package/my-tool-git|github.com/example/my-tool|a git package links to its repository"
    "/package/my-tool-git|A tool built straight from git|a git package has metadata from its checkout"
    "/settings|card-title\">Settings|settings"
    "/config-files|card-title\">Config files|config files"
    "/workers|card-title\">Workers|workers"
    "/activities|card-title\">Activities|activities"
    "/no/such/page|Not found|404 (depth 3)"
)
if [ "$ONLINE" = "1" ]; then
    # Fetches the PKGBUILD from the AUR, so it needs network.
    ROUTES+=("/package/hello/source/PKGBUILD|Revert to upstream|source editor (depth 4)")
fi

[ -n "$SHOTS" ] && mkdir -p "$SHOTS"

echo "==> checking routes"
failures=0
for entry in "${ROUTES[@]}"; do
    route="${entry%%|*}"; rest="${entry#*|}"
    marker="${rest%%|*}"; desc="${rest#*|}"

    dom="$("$CHROME" --headless --disable-gpu --no-sandbox \
             --virtual-time-budget=8000 --dump-dom "http://localhost:$UI_PORT$route" 2>/dev/null || true)"

    # The shell is the layout every route renders into. Missing it means the
    # app never mounted -- a blank page -- rather than a wrong screen.
    if ! printf '%s' "$dom" | grep -q "drawer-side"; then
        echo "  FAIL  $route ($desc): app did not mount"
        failures=$((failures + 1))
        continue
    fi
    if ! printf '%s' "$dom" | grep -q "$marker"; then
        echo "  FAIL  $route ($desc): mounted, but did not render $marker"
        failures=$((failures + 1))
        continue
    fi
    echo "  ok    $route ($desc)"

    if [ -n "$SHOTS" ]; then
        name="$(printf '%s' "${route#/}" | tr '/' '-')"
        [ -z "$name" ] && name="index"
        "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
            --window-size=1440,1400 --virtual-time-budget=8000 \
            --screenshot="$SHOTS/$name.png" "http://localhost:$UI_PORT$route" >/dev/null 2>&1
    fi
done

# The narrow layout drops columns rather than scrolling them, and that rule has
# broken twice. Worth one shot rather than only a class-name assertion.
if [ -n "$SHOTS" ]; then
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --window-size=420,900 --virtual-time-budget=8000 \
        --screenshot="$SHOTS/packages-narrow.png" "http://localhost:$UI_PORT/packages" >/dev/null 2>&1
    echo "==> screenshots in $SHOTS"
fi

if [ "$failures" -gt 0 ]; then
    fail "$failures route(s) did not render"
fi
echo "==> all ${#ROUTES[@]} routes rendered"
