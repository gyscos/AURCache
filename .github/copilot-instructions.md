# Copilot Instructions

## Build, test, and lint commands

```bash
# repo helpers
just format
just lint
just codegen

# Rust backend workspace
cd backend
cargo fmt -- --check
cargo clippy -- -D warnings
cargo check
cargo test --all

# run a single Rust integration test
cargo test -p aurcache-db --test dependency_backfill backfill_creates_dependency_links
cargo test -p aurcache-utils --test add scenario_b_one_aur_dep

# Flutter frontend
cd frontend
flutter pub get
flutter pub run build_runner build --delete-conflicting-outputs
flutter analyze --no-fatal-infos
dart format --set-exit-if-changed .
flutter test

# run a single Flutter test file
flutter test test/widget_test.dart

# builds used elsewhere in the repo
cd frontend && flutter build web
cd docs && yarn install --frozen-lockfile && yarn build

# end-to-end smoke test
./scripts/test-e2e.sh hello
```

## High-level architecture

- `backend/` is a Cargo workspace. `backend/aurcache` is the composition root: it loads env, initializes the database and migrations, runs startup cleanup, starts the build queue and schedulers, then launches the API server and the repository file server.
- `backend/aurcache-api` owns the Rocket HTTP surface. Route registration is centralized in `src/backend.rs`, OpenAPI docs are assembled in `src/init.rs`, and the app serves docs at `/docs` and `/redoc`.
- `backend/aurcache-utils` holds most package/business logic. Package add/update flows resolve AUR or git sources, create package and dependency rows, and send build actions onto the broadcast queue instead of building inline.
- `backend/aurcache-builder` executes containerized builds, streams logs, updates build/package status, and triggers dependent rebuilds only after dependency versions satisfy recorded constraints.
- `backend/aurcache-db` owns SeaORM entities and migrations. `init_db()` applies migrations on startup; SQLite uses `./db` plus WAL pragmas, while Postgres is selected from env vars.
- `backend/aurcache-scheduler` contains background jobs such as auto-update, mirror ranking, and version checking.
- `frontend/` is a Flutter web UI. Navigation is in `lib/components/routing/router.dart`, HTTP access is via Dio in `lib/api`, async state is exposed through Riverpod providers in `lib/providers`, and typed API models live in `lib/models`.
- `docs/` is a separate Docusaurus site used for published documentation.

## Key conventions

- Keep changes in the owning crate instead of piling logic into `backend/aurcache`: HTTP schema/handlers belong in `aurcache-api`, persistence and migrations in `aurcache-db`, build orchestration in `aurcache-builder`, schedulers in `aurcache-scheduler`, and package/settings helpers in `aurcache-utils`.
- Package addition is dependency-first. `aurcache_utils::package::add` recursively resolves AUR `depends` and `make_depends`, inserts dependency links, marks only the originally requested package as `directly_requested`, and initially enqueues only leaf packages.
- Successful builds fan out through the dependency graph. `aurcache-builder` checks recorded dependency constraints and only triggers dependents when all dependency builds are ready and version-compatible.
- Several persisted fields are encoded strings rather than richer DB types: `platforms` and `build_flags` are semicolon-delimited, while `source_data` and `split_packages` are JSON strings. Preserve those encodings when touching DB, API, or model conversion code.
- Settings are resolved through `ApplicationSettings` helpers, not by reading env vars ad hoc. The effective precedence in code is `Package -> Env -> Global -> Default`.
- Frontend data/state layers depend on code generation. Models use `json_serializable` and `freezed`; providers use `@riverpod`. After editing annotated Dart files, rerun `flutter pub run build_runner build --delete-conflicting-outputs`.
- Generated Dart files (`*.g.dart`, `*.freezed.dart`) are analyzer-excluded; edit the source file, not the generated output.
- The frontend talks to `http://localhost:8080/api` in debug builds and resolves `api/` relative to the current origin in release/web builds.
- The repo includes multiple containerized workflows: `docker-compose.hostmode.dev.yaml` mounts the host Docker socket for builds, `docker-compose.dindmode.dev.yaml` is the simpler dev setup, and `scripts/test-e2e.sh` exercises `docker-compose.e2e.yaml` end to end.
