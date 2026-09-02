# Development
If you want to contribute to the project feel free to checkout the code and try to fix a bug or implement a feature.

## Build Info

The AURCache project comprises two main components, both Rust: a Dioxus frontend
compiled to WebAssembly and the backend.

### Frontend (Rust / Dioxus)

`frontend-rs/` is its own Cargo workspace, because it only builds for
`wasm32-unknown-unknown` and including it in the backend workspace would break
`cargo build --workspace`.

```bash
cd frontend-rs
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test   # host target: the component logic, not the browser
```

There is no separate build step for it. `aurcache-api`'s build script compiles
the frontend to wasm and embeds it under the `static` feature, re-running
whenever the frontend changes:

```bash
cd backend && cargo run --features aurcache-api/static -p aurcache
```

### Backend (Rust)

To build the Rust backend, make sure you have Rust installed. Then, navigate to the backend directory and run:

```bash
cd backend
cargo build --release
```

#### `alpm-pkgbuild-bridge`

The backend needs the [`alpm-pkgbuild-bridge`] script on your `$PATH` to parse
PKGBUILDs. It is **not** bundled with any crate: `alpm-srcinfo` shells out to
it, and looks it up in `$PATH` at runtime.

```bash
# Arch
sudo pacman -S alpm-pkgbuild-bridge

# anything else
sudo curl -sSLf -o /usr/local/bin/alpm-pkgbuild-bridge \
  "https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads"
sudo chmod 755 /usr/local/bin/alpm-pkgbuild-bridge
```

Without it you get `PKGBUILD parsing failed and no fixes were applied` — but
only for *patched* sources, which makes it look like a code bug rather than a
missing tool. An unpatched package reads its shipped `.SRCINFO` and works fine;
applying a patch invalidates that `.SRCINFO` and forces a PKGBUILD re-parse
through the bridge. So most of the suite passes and a handful of patch-related
tests fail.

The Docker image installs it the same way (see `docker/Dockerfile`), so this
only affects running the backend or its tests directly on your machine.

[`alpm-pkgbuild-bridge`]: https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge

## Tests and lints

These are what CI runs, so it is worth running them before opening a PR:

```bash
# backend — needs alpm-pkgbuild-bridge (see above)
cd backend
cargo fmt -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# frontend
cd frontend-rs
cargo fmt -- --check
cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test
```

The repo root has a `Justfile` with `just format`, `just lint`, and
`just codegen` for convenience, but note they are more lenient than CI —
`just lint` runs `cargo clippy` without `-D warnings`, so it can pass where CI
fails. Use the commands above when checking whether a change is ready.

### API types are shared, not mirrored

There is no code generation step. The types the API speaks live in
`aurcache-common` and are used directly by the server, the CLI and the
frontend, which reaches them through the same `aurcache-client` crate the CLI
uses. A field added to a response is added once and all three see it — mirroring
a struct by hand is how the two ends drift apart.

### Api Docs
You can access the API docs (scalar) `http://localhost:8080/docs` after starting the backend.

Or if you prefer Redoc `http://localhost:8080/redoc`.