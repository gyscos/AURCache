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
# correctly — and broke only `/package/hello/build/1` and below. A list of shallow routes
# would have passed straight through it.
ROUTES=(
    "/|Builds per month|dashboard"
    # The tiles that link somewhere only render with a router above them, which
    # the unit tests cannot stand up — so this is where they are covered.
    "/|Repository|the dashboard shows its headline numbers"
    "/|href=\"/packages\"|a counted tile links to what it counts"
    # Dependencies are counted apart from what was asked for. The fixture has
    # 10 requested and 2 dependency-only packages; both tiles are links, so
    # this is the only place they render.
    "/|12 with dependencies|dependencies are counted apart from requests"
    # A bad week must not hide inside a good lifetime figure.
    "/|this week|the weekly reading sits beside the lifetime one"
    # The chart is SVG, so a real element proves it drew rather than errored.
    "/|<svg|the build graph renders"
    "/builds|Duration|builds list"
    "/package/hello/build/1|hello|build log (depth 4)"
    "/packages|Upstream|packages list"
    "/packages|—|missing version shows a placeholder"
    "/packages/add|AUR package name or git URL|the add dialog (depth 2)"
    # A search in the URL is applied on arrival, so a link to one opens with it
    # already narrowed rather than showing everything.
    # A search in the URL is applied on arrival, so a link to one opens
    # narrowed rather than showing everything. `1 of 10` is the proof it
    # filtered: the name alone would render on the unfiltered page too.
    "/packages#hello|1 of 10|a linked search opens applied"
    "/packages#aewm%2B%2B|1 of 10|a term needing escapes survives the URL"
    "/packages/add|Applied to every package above|one platform choice covers the whole queue"
    # The list stays mounted behind the dialog, so dismissing it reveals the
    # page as it was rather than reloading it.
    "/packages/add|Filter packages|the add dialog leaves the list behind it"
    "/package/hello|Dependencies|package detail (depth 2)"
    "/package/hello|In repo|package detail shows what is in the repo"
    "/package/hello|Rebuild|the rebuild button sits with the builds"
    "/package/hello|Change|platforms can be edited where they are shown"
    "/package/hello/builds|Platform|per-package build history (depth 3)"
    "/package/yay|blocking|a blocked package names what is holding it back"
    "/package/yay|too old|a dependency built to an unsatisfying version"
    "/package/my-tool-git|github.com/example/my-tool|a git package links to its repository"
    "/package/my-tool-git|A tool built straight from git|a git package has metadata from its checkout"
    "/settings|Version check interval|settings"
    # The fixture server runs with VERSION_CHECK_INTERVAL set, so this row is
    # env-locked. Naming the variable proves the source made it all the way
    # from the API to the screen, and the wording has to say what to do about
    # it — the field is disabled and the variable is not set from this page.
    "/settings|unset \$VERSION_CHECK_INTERVAL|an env-pinned setting says how to take it back"
    # Named on every row, set or not, so the page documents what a deployment
    # can pin rather than only reporting what it already pinned.
    "/settings|\$JOB_TIMEOUT|settings document their environment variables"
    "/settings|Builder image|settings covers every section"
    # Seeded as a stored global value, which is the only state offering a Reset.
    "/settings|>Reset<|settings can undo a stored value"
    "/config-files|makepkg.conf|config files"
    # That the stored file reaches the editor is checked in the interaction
    # tests instead: a textarea's value is a DOM property, and a dump of the
    # serialised document does not carry it.
    # Stored and unset render differently; only the stored one offers a Reset.
    "/config-files|>stored<|a stored file says so"
    "/workers|card-title\">Workers|workers"
    "/activities|added package hello|activities"
    # The text is rendered server-side from the stored JSON, so this also
    # proves the payload shapes in the fixture are ones the server can parse.
    "/activities|forced update of package yay|the log renders each entry type"
    # Nobody asked for this one; a schedule did. The Dart frontend called that
    # "You", which claims work the reader did not do.
    "/activities|AURCache|an unattributed entry is credited to the server"
    "/no/such/page|Not found|404 (depth 3)"
)
if [ "$ONLINE" = "1" ]; then
    # Fetches the PKGBUILD from the AUR, so it needs network.
    ROUTES+=("/package/hello/source/PKGBUILD|Revert to upstream|source editor (depth 4)")
    ROUTES+=("/package/hello/source/PKGBUILD|Save &amp; Rebuild|save and queue in one step")
    # The way back is the breadcrumb's package link. It used to be a "← hello"
    # button; this marker went stale when the heading became the trail, and no
    # one noticed because these routes only run with --online.
    ROUTES+=("/package/hello/source/PKGBUILD|href=\"/package/hello\"|the editor offers a way back")
    ROUTES+=("/package/hello/source/PKGBUILD|>Sources<|the editor says where it is")
    # The add dialog offers the same editor for a package that does not exist
    # yet, which is the only way to add one whose PKGBUILD will not parse.
    ROUTES+=("/packages/add#hello|Edit sources before adding|the add dialog can patch one source")
fi

# Things that mean someone was midway through diagnosing something. Deliberately
# narrow: this has to be quiet on real content, and package descriptions are
# arbitrary upstream text.
JUNK='PROBE\[|DEBUG\[|HREF\[|QUERY\[|dbg!|todo!\(|XXXTEMP'

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

    # Every check above asks whether something is present. None of them can
    # notice something that should not be, which is how a debug probe rendering
    # `HREF[...]QUERY[...]` above two list pages survived four commits: the
    # markers it sat beside still matched. This asks the other question.
    if junk="$(printf '%s' "$dom" | grep -oE "$JUNK" | sort -u | head -3)" && [ -n "$junk" ]; then
        echo "  FAIL  $route ($desc): left-over debugging in the page: $(printf '%s' "$junk" | tr '\n' ' ')"
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

# Two shots at widths the default 1440 does not cover.
#
# The narrow layout drops columns rather than scrolling them, and that rule has
# broken twice. Settings splits into two columns only above 1440, so the shape
# most people actually see it in is not the shape the run above captured.
if [ -n "$SHOTS" ]; then
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --window-size=420,900 --virtual-time-budget=8000 \
        --screenshot="$SHOTS/packages-narrow.png" "http://localhost:$UI_PORT/packages" >/dev/null 2>&1
    "$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
        --window-size=1920,1200 --virtual-time-budget=8000 \
        --screenshot="$SHOTS/settings-wide.png" "http://localhost:$UI_PORT/settings" >/dev/null 2>&1
    echo "==> screenshots in $SHOTS"
fi

if [ "$failures" -gt 0 ]; then
    fail "$failures route(s) did not render"
fi
echo "==> all ${#ROUTES[@]} routes rendered"

# The checks above dump a page and read it. These drive it: type in a filter,
# queue something, take it back. That needs a session rather than one
# --dump-dom per assertion, so it lives in Rust. The browser is the test
# crate's business — it resolves Chrome, fetches a matching chromedriver and
# stops both — so there is nothing to start or clean up here.
echo "==> checking interactions"
( cd "$PROJECT_DIR/frontend-rs" \
    && AURCACHE_UI="http://localhost:$UI_PORT" \
       timeout 240 cargo test --quiet --test browser -- --ignored ) \
    || fail "interaction tests failed"
