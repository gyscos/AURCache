# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

AURCache is a build server and repository for Arch Linux packages sourced from the AUR. Backend and
frontend are both Rust. Users add AUR (or git) packages for building; AURCache builds them in
containers and serves the results as a pacman repository, and detects when packages are out of date.

`frontend-rs/` (Dioxus, compiled to wasm) is the frontend. A Flutter UI used to live in `frontend/`; it
was removed once it had diverged past usefulness, and `git log` is where to find it if ever needed.

## Build, test, and lint commands

```bash
# repo helpers (Justfile at repo root) -- these are the whole workflow
just serve         # run a server with the UI embedded, as the container ships it
just format        # cargo fmt, both workspaces
just lint          # clippy over both workspaces; the frontend for wasm *and* host
just test          # cargo test, both workspaces (no browser)
just test-browser  # scripts/test-frontend.sh
just clean

# Rust backend workspace
cd backend
cargo fmt -- --check
cargo clippy -- -D warnings
cargo check
cargo test --all

# run a single Rust integration test
cargo test -p aurcache-db --test dependency_backfill backfill_creates_dependency_links
cargo test -p aurcache-utils --test add scenario_b_one_aur_dep

# Rust frontend (its own workspace: it only builds for wasm32-unknown-unknown,
# so including it in the backend workspace would break `cargo build --workspace`)
cd frontend-rs
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test          # host target; the component logic, not the browser

# There is no separate frontend build step. `aurcache-api`'s build script
# compiles the frontend to wasm and embeds it under the `static` feature,
# re-running whenever the frontend changes.
cd backend && cargo run --features aurcache-api/static -p aurcache

# builds used elsewhere in the repo
cd docs && yarn install --frozen-lockfile && yarn build

# Rust frontend in a real browser: asserts every route mounts (~1 min)
./scripts/test-frontend.sh
./scripts/test-frontend.sh --shots /tmp/shots   # also write screenshots
./scripts/test-frontend.sh --online             # include routes that fetch from the AUR

# end-to-end smoke test
./scripts/test-e2e.sh hello
```

### Which end-to-end suite to run

`scripts/test-e2e.sh` (the split server/worker path, ~4 min) is the default and
is what CI runs. Reach for the others only when a change touches what they
cover, rather than running all four as a matter of course:

- `test-e2e-hybrid.sh` / `test-e2e-hybrid-legacy.sh` — only for changes to the
  hybrid image, the chroot builder, or the docker builder.
- `test-sandbox.sh` — only for changes to `aurcache-sandbox` or the PKGBUILD
  sourcing path.

Unit and integration tests are cheap (backend ~8s, frontend <1s) and should be
the reflex. They render components directly, though, so they cannot see whether
the page boots — `test-frontend.sh` is what covers that, and every rendering
defect this frontend has had was of that kind.

## High-level architecture

- `backend/` is a Cargo workspace. `backend/aurcache` is the composition root: it loads env, initializes
  the database and migrations, runs startup cleanup, starts the build queue and schedulers, then launches
  the API server and the repository file server.
- `backend/aurcache-api` owns the Rocket HTTP surface. Route registration is centralized in
  `src/backend.rs`, OpenAPI docs are assembled in `src/init.rs`, and the app serves docs at `/docs` and
  `/redoc`.
- `backend/aurcache-utils` holds most package/business logic. Package add/update flows resolve AUR or git
  sources, create package and dependency rows, and send build actions onto the broadcast queue instead of
  building inline.
- `backend/aurcache-builder` executes containerized builds, streams logs, updates build/package status,
  and triggers dependent rebuilds only after dependency versions satisfy recorded constraints.
- `backend/aurcache-db` owns SeaORM entities and migrations. `init_db()` applies migrations on startup;
  SQLite uses `./db` plus WAL pragmas, while Postgres is selected from env vars.
- `backend/aurcache-scheduler` contains background jobs such as auto-update, mirror ranking, and version
  checking.
- `backend/aurcache-cli` is a typed CLI for the HTTP API (`cargo run -p aurcache-cli -- ...`); the reusable
  Rust client library backing it lives in `backend/aurcache-client`. It authenticates via
  `Authorization: Bearer <token>`, resolving `--url`/`--token`, then `AURCACHE_URL`/`AURCACHE_TOKEN`, then
  `~/.config/aurcache-client/config.json`, then an interactive prompt (saved back to the config file). Use
  `--format json` for machine-readable output, or `raw` for endpoints without a dedicated subcommand.
- `backend/aurcache-common` is the shared leaf crate: the API types the server, CLI and browser frontend
  all speak, plus small helpers that would otherwise be duplicated or force a heavy dependency. Everything
  in it is dependency-free or behind a feature — `db` (sea-orm derives) and `fs` (filesystem helpers) are
  both on by default, and wasm consumers take `default-features = false` and still get the types. CI's
  "Check driver-free types" job (`cargo check -p aurcache-common --no-default-features`) is what keeps
  that true; a workspace build always turns the features on, so nothing else would catch a regression.
  It was called `aurcache-types` until it grew helpers as well as types.
- `frontend-rs/` is the Dioxus UI, in its own workspace because it only builds for
  `wasm32-unknown-unknown`. Routes are in `src/routes.rs`, the sidebar and layout in `src/shell.rs`,
  screens in `src/screens/`, and the pure filter/sort/paginate helpers in `src/listing.rs` — those are
  where list behaviour is tested, since components cannot be rendered in a unit test. The API client is
  `aurcache-client`, the same one the CLI uses, so API shapes are shared rather than re-declared.
- `docs/` is a separate Docusaurus site used for published documentation.

## Key conventions

- Keep changes in the owning crate instead of piling logic into `backend/aurcache`: HTTP schema/handlers
  belong in `aurcache-api`, persistence and migrations in `aurcache-db`, build orchestration in
  `aurcache-builder`, schedulers in `aurcache-scheduler`, and package/settings helpers in `aurcache-utils`.
- Package addition is dependency-first. `aurcache_utils::package::add` recursively resolves AUR `depends`
  and `make_depends`, inserts dependency links, marks only the originally requested package as
  `directly_requested`, and initially enqueues only leaf packages.
- Successful builds fan out through the dependency graph. `aurcache-builder` checks recorded dependency
  constraints and only triggers dependents when all dependency builds are ready and version-compatible.
- Several persisted fields are encoded strings rather than richer DB types: `platforms` and `build_flags`
  are semicolon-delimited, while `source_data` and `split_packages` are JSON strings. Preserve those
  encodings when touching DB, API, or model conversion code.
- Settings are resolved through `ApplicationSettings` helpers, not by reading env vars ad hoc. The
  effective precedence in code is `Package -> Env -> Global -> Default`.
- API shapes live in `aurcache-common` and are shared, never re-declared. A field added to a response is
  added once and the CLI, the client library and the frontend all see it; mirroring a struct by hand is
  how the two ends drift apart.
- The frontend resolves the API against the origin the page was served from, falling back to
  `http://localhost:8080/api` when there is no window (see `frontend-rs/src/api.rs`).
- A size, a count or a total that is not known is `Option::None`, not `0`: "nothing recorded" and "zero
  bytes" are different answers, and the UI renders the first as a dash. Totals over several such values
  are all-or-nothing — a sum of only the known parts reads as a wrong number rather than as missing data.
- The repo includes multiple containerized workflows: `docker-compose.hostmode.dev.yaml` mounts the host
  Docker socket for builds, `docker-compose.dindmode.dev.yaml` is the simpler dev setup, and
  `scripts/test-e2e.sh` exercises `docker-compose.e2e.yaml` end to end.
