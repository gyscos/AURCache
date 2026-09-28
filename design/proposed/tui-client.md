# Design: A Terminal Client (`aurcli tui`)

Status: **Proposed** · Last updated: 2026-09-28

## Motivation

AURCache has two ways in today: the web UI, which shows everything but needs a
browser, and `aurcli`, which does everything but shows one answer per
invocation. Watching an instance — the queue draining, a build's log scrolling,
a worker dropping off — falls between them. Over SSH on the machine that runs
the server, or on a laptop without a tab to spare, the only option is
`aurcli builds watch` and re-running `aurcli` commands by hand.

A TUI built on [cursive](https://github.com/gyscos/cursive) fills that gap: the
web UI's screens, in a terminal, kept fresh the way the web UI keeps them fresh,
with the same actions one keystroke away. It is a third client of the same API,
not a new surface: every request it makes already exists in `aurcache-client`,
and every shape it reads is already in `aurcache-common`.

### Goals

- Replicate the web UI's monitoring screens — dashboard, builds, packages, one
  package, one build's log, the event log, workers, settings — closely enough
  that someone who knows one finds their way in the other.
- The everyday actions: rebuild, update, retry, cancel, remove, approve and
  revoke a worker, edit a setting, add a package.
- No new endpoint. If the TUI needs data the web UI does not already fetch, that
  is a sign the screen is being designed differently, not that the API is short.
- No duplicated presentation logic. Filtering, sorting, date and duration
  formatting and log paging are already written and tested once, in
  `frontend-rs`; the TUI uses the same code rather than a second copy.

### Non-goals

- Pixel parity. A terminal has no hover text, no charts worth the name, and no
  second pane on an 80-column screen; each screen is redesigned for what a
  terminal is good at, keeping the *information* and the *question it answers*.
- Backup and restore. A one-off job that reads or writes a file is what
  `aurcli dump` / `aurcli restore` already are; the TUI points at them.
- Several instances at once. One client, one server, as for `aurcli`.

---

## Part 1 — Where it lives

### A library crate, launched by `aurcli`

New crate `backend/aurcache-tui`, in the backend workspace (it builds for the
host, and tokio is already there). It is a library exposing roughly:

```rust
pub fn run(client: AurCacheClient, start: Option<Screen>) -> anyhow::Result<()>;
```

and `aurcache-cli` gains a `tui` subcommand behind a default-on `tui` feature
that calls it.

A subcommand rather than a separate binary because everything in front of the
first request is already solved in `aurcli`: `--url`/`--token`, the
environment, `~/.config/aurcache-client/config.json`, the interactive token
prompt that saves back to it (`config::resolve_runtime_config`,
`prompt_and_save_token`). A second binary would either duplicate that or grow
a dependency on the CLI crate anyway. It also means no new AUR package: the
`aurcache-cli` PKGBUILD ships it as is.

A library rather than a module of the CLI crate because `main.rs` is already
~140K and the TUI is a program of its own; the crate boundary also keeps
cursive out of anything that turns the feature off.

**Backend: crossterm.** No system ncurses to link, and it is what builds
everywhere the CLI builds.

### Presentation helpers move to `aurcache-common`

These `frontend-rs` modules are either pure already or pure apart from a thin
Dioxus layer:

| Module | What the TUI needs from it | Dioxus in it |
|---|---|---|
| `listing.rs` | filter/sort/paginate for packages and builds, the search grammar | two link-building helpers taking a `Route` |
| `dates.rs` | relative ("3h ago") and absolute formatting, the format preference | the hover-title component |
| `format.rs` | durations, sizes, build timing | none |
| `log_tail.rs` | `append_capped`, the tail window arithmetic | none (`log_tail_cap` measures the browser) |
| `status.rs` | a build state's label and severity | the badge component |
| `platforms.rs` | the list of architectures offered | the picker component |

Step one of this work is to move the pure halves into `aurcache-common` (as
`aurcache_common::listing`, `::dates`, `::format`, …), their tests with them,
and have `frontend-rs` import them from there. The frontend's behaviour does
not change; its component halves stay where they are. Anything that reads a
clock takes `now` as a parameter — the browser's comes from `js-sys`, the
TUI's from `SystemTime` — which is how the tests already drive it.

This is exactly what `aurcache-common` is for ("small helpers that would
otherwise be duplicated"), and it is dependency-free code, so the
driver-free check keeps passing. The alternative — a third crate, say
`aurcache-present` — is only worth it if `listing.rs` at ~1900 lines feels out
of place next to the API types.

---

## Part 2 — Architecture

### Two threads, one direction each

cursive owns the main thread and its event loop; the client is async. A
tokio runtime runs on a background thread and holds the one `AurCacheClient`.
The two talk in one direction each:

- UI → runtime: `runtime.spawn(fetch)`, from a callback, with a clone of the
  client.
- runtime → UI: the fetch ends by sending a closure through
  `Cursive::cb_sink()`, which applies the result to the view tree on the UI
  thread.

No view is ever touched from the runtime, and nothing on the UI thread
awaits. A slow server makes the data late, never the keyboard.

### A screen is a fetch plus a render

Each screen is a type implementing roughly:

```rust
trait Screen {
    type Data: Send + 'static;
    /// What this screen fetches, joined: the package page is two requests.
    fn load(&self, client: AurCacheClient) -> BoxFuture<'static, Result<Self::Data>>;
    /// Build the view the first time.
    fn view(&self) -> BoxedView;
    /// Apply fresh data to the existing view, in place.
    fn apply(&self, siv: &mut Cursive, data: &Self::Data);
    /// Whether anything on screen is in flight -- drives the poll interval.
    fn busy(data: &Self::Data) -> bool;
}
```

`apply` updates in place (`call_on_name`) rather than replacing the view, so
a refresh keeps the cursor on the same row and the scroll where it was. Rows
are matched by key — pkgbase, build id, worker id — not by index, so a new
build at the top of the list does not move the cursor onto a different one.
This is the part the web UI gets for free from a virtual DOM and the TUI has
to do on purpose.

Splitting `load` from `apply` is also what makes screens testable: `apply`
runs against fixture data with no server (see Part 5).

### Navigation: a stack, like the browser's history

A screen stack, one cursive layer per screen: `Enter` on a row pushes the
row's screen, `Esc` pops back to where you were with the cursor where you left
it. Sections — Dashboard, Builds, Packages, Log, Workers, Settings — are
reached from anywhere with a key (see Part 4), which replaces the stack rather
than pushing onto it, the way the web UI's sidebar does.

A status line at the bottom is the frame the web UI's shell is: the instance
URL, when the current screen last refreshed, whether the server answered, and
the keys the current screen accepts.

The TUI's `Screen` enum names the same things the web UI's `Route` does, and
`aurcli tui` takes an optional start screen in the same path form:

```
aurcli tui /package/yay/build/12
aurcli tui https://aur.example.com/workers   # a URL pasted from the browser
```

so a link from the web UI, a notification or a log opens in the terminal at the
same place.

### Keeping it fresh

The same policy as `frontend-rs/src/poll.rs`: the screen on top re-fetches
every 5 s while `busy` says something is in flight, every 60 s when nothing
is. Screens under it do not poll; popping back to one re-fetches it at once.
Leaving a screen cancels its poll task (an `AbortHandle` held with the layer).
`r` refreshes now.

The web UI stops polling in a hidden tab. The terminal equivalent is focus
reporting; crossterm can emit focus events, but cursive does not pass them on
today. Worth doing if cheap; otherwise a TUI left open idles at one request a
minute, which is fine.

### Errors

- **Unreachable server or 5xx:** keep showing the last data, and say in the
  status line that it is stale and since when. A monitoring tool that blanks
  the screen on a blip is worse than one that shows data a minute old.
- **401:** the client's `on_unauthorized` hook opens a dialog for a new token,
  saved the way `aurcli` saves one.
- **404 on a detail screen** (a package removed while you were on it): pop to
  the list it belongs to with a notice saying why — the same rule as
  `frontend-rs/src/notice.rs`.
- **A failed action:** a dialog with the server's message (`ApiError` already
  normalises it), leaving the screen as it was.

---

## Part 3 — The screens

Each row names the web screen it replicates. "Read" is what it shows; "Act" is
what it can do from there.

| Screen | Web counterpart | Read | Act |
|---|---|---|---|
| Dashboard | `dashboard.rs` | stats, recent problems, recent packages/builds, failed, out of date (needing a hand vs handled), stuck queue, largest, longest; the build graph as a bar sparkline | open any row |
| Builds | `builds.rs` | the build list, with the web UI's filter grammar and sort | retry, cancel |
| Packages | `packages.rs` | the package list, same filter grammar, status per architecture | update, rebuild, retry, add (`a`) |
| Package | `package.rs`, `package_builds.rs` | the health header — per architecture, the latest build and the one the repository serves; dependencies and dependents; source; flags and platforms; the full build history as a tab | rebuild, update, edit flags/platforms, replace a dependency, remove |
| Build | `build.rs` | the log, tailed and followed; state, timing, worker | retry, cancel, delete, save the whole log to a file |
| Log | `logs.rs` | the event log, filtered server-side by severity, kind, entity, since boot | open what an entry names |
| Workers | `workers.rs` | the fleet, with machines awaiting approval called out; retired ones behind a toggle | approve, pause, resume, revoke |
| Worker | `worker.rs` | what it is, what it has done, its settings with where each value came from and which it refused | edit a setting, reset one |
| Settings | `settings.rs`, `config_files.rs` | every server setting with its value and origin (package → env → global → default) | edit, reset; edit `makepkg.conf`/`pacman.conf` |
| Add | `package_add.rs` | AUR search results; bulk-add progress | queue several, pick platforms, add |

Screen-specific notes:

**Lists.** A `SelectView` whose rows are column-aligned `StyledString`s,
re-laid-out on resize, or `cursive_table_view` for sortable headers — see
Open questions. Either way the rows come from `aurcache_common::listing`, so
`/` takes the same filter text as the web UI's search box, and a filter typed
in one works in the other.

**Status.** A build state is a coloured word, from the shared label and
severity. The palette follows the terminal's own colours
(`Palette::terminal_default()`-style) rather than imposing a theme.

**The build log.** The web screen's model carries over: fetch a bounded tail
with `build_output_page`, walk it forward in pages aligned by
`aurcache_common::api::build_log::align`, keep the window under a cap by
dropping whole leading lines with the shared `append_capped`. The view is a
`TextView` over a `TextContent` in a `ScrollView` with
`ScrollStrategy::StickToBottom`, which is "Follow": scrolling up stops
following, `End` resumes it. The cap is a fixed number of bytes rather than a
browser measurement — a terminal's scrollback cost is the `String`, nothing
else. `s` saves the full log (not just the window) to
`./<pkgbase>-<number>.log`, standing in for the web UI's Download.

**The dashboard graph.** `GraphDataPoint`s as one row of `▁▂▃▄▅▆▇█` per
series with the axis labels at the ends. Enough to see "builds stopped
yesterday", which is all the graph is for at a glance.

**Adding a package.** One dialog, as on the web: a source field that tells a
package name from a git URL with `looks_like_git_url`, AUR search results
under it, a queue of several, one platform picker for all of them. On submit
the dialog closes and the bulk-add progress becomes a line in the status bar
(`bulk_add_progress` read by offset), which can be opened into a dialog or
dismissed — dismissing stops watching, not adding, as in `progress.rs`.
Previewing and editing an upstream source *before* adding it is left to the
web UI in the first version.

**Editing files.** Package sources and the two config files are files, and a
terminal user has an editor. The TUI leaves the cursive event loop, runs
`$VISUAL`/`$EDITOR` on a temporary copy, and re-enters the loop; if the file
changed, it is sent with `put_source_file` / `patch_setting`. With no editor
configured, a full-screen `TextArea` stands in.

**Dates.** Relative or absolute is the screen's choice, as on the web; how an
absolute date reads is the server's default, overridable by a
`date_format` field in the client config file (the TUI's equivalent of the
browser-local override).

---

## Part 4 — Keys

The same everywhere, so nothing needs learning twice:

| Key | Does |
|---|---|
| `↑↓` / `jk`, `PgUp PgDn`, `g G` | move |
| `Enter` | open |
| `Esc` / `Backspace` | back |
| `d b p l w s` | Dashboard, Builds, Packages, Log, Workers, Settings |
| `/` | filter (lists), search (log) |
| `r` | refresh now |
| `?` | every key the current screen accepts |
| `q` | quit (asks nothing: there is never unsaved state outside a dialog) |

Actions are capitals, so a stray lowercase key never changes anything: `R`
rebuild, `U` update, `T` retry, `C` cancel, `X` remove, `A` approve, `V`
revoke, `E` edit. Anything destructive — remove, revoke, delete, cancel —
confirms in a dialog that names what it is about to do, the way the web UI's
confirmations do.

The status line lists the current screen's actions, so `?` is for
completeness rather than discovery.

---

## Part 5 — Testing

- **The moved helpers** keep their tests, now running in the backend
  workspace's `cargo test`.
- **Each screen's `apply`** is tested against fixture data with cursive's
  puppet backend: build the view, apply a fixture, render, assert on the
  screen text. This is the TUI's counterpart of `test-frontend.sh`'s "every
  route mounts" — the class of defect that bit the web UI was a screen that
  failed to come up at all, and a puppet render catches that here without a
  terminal.
- **Refresh keeps the cursor:** apply data, move the cursor, apply data with a
  row inserted above, assert the cursor is on the same key.
- **Start-screen parsing:** every path form the web UI's `Route` produces
  parses to the matching `Screen`. The two live in different workspaces, so
  this is a table of paths in the TUI's tests, not a shared type.

No test drives a real server; the client's requests are already exercised by
the CLI and the e2e suite.

---

## Part 6 — Order of work

1. **Move the helpers** to `aurcache-common`; `frontend-rs` imports them.
   Behaviour-neutral, and valuable even if the TUI stopped here.
2. **The skeleton:** the crate, `aurcli tui`, the runtime bridge, the screen
   stack, the status line, polling. Dashboard, Builds and Build (the log) —
   read-only. This is already the monitoring tool.
3. **The rest of the reading:** Packages, Package, Log, Workers, Worker,
   Settings.
4. **Actions:** rebuild/update/retry/cancel/remove, approve/pause/resume/revoke,
   setting edits.
5. **Adding packages, and editing files** through `$EDITOR`.

---

## Open questions

- **Tables.** `SelectView` with hand-aligned columns keeps the dependency list
  short and is enough without sortable headers — sorting can live in the
  filter grammar, as it does in the URL. `cursive_table_view` gives headers
  and column sorting but is another dependency to track. Leaning towards
  `SelectView`.
- **Helpers: `aurcache-common` or a new crate?** Common is the natural home by
  its own description; a separate crate only if the listing code feels out of
  place there.
- **Suspending for `$EDITOR`.** Quitting the run loop and running it again
  re-initialises the backend; whether that is clean with crossterm (alternate
  screen, raw mode, pending input) needs trying. A cursive-level
  "suspend/resume the backend" would be the tidier answer and might be worth
  adding upstream.
- **Focus events** for pausing the poll, as above: cheap in crossterm, not
  surfaced by cursive today.
- **Mouse.** cursive supports it and it costs nothing to leave on; clicking
  rows is harmless. Worth confirming it does not fight terminal selection for
  copying a line out of a build log.
