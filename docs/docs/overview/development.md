# Development
If you want to contribute to the project feel free to checkout the code and try to fix a bug or implement a feature.

## Build Info

The AURCache project comprises two main components: a Flutter frontend and a Rust backend.
### Frontend (Flutter)

To build the Flutter frontend, ensure you have Flutter SDK installed. Then, execute the following commands:

```bash
cd frontend
flutter pub get
flutter build web
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
cd frontend
dart format --set-exit-if-changed .
flutter analyze --no-fatal-infos
flutter test
```

The repo root has a `Justfile` with `just format`, `just lint`, and
`just codegen` for convenience, but note they are more lenient than CI —
`just lint` runs `cargo clippy` without `-D warnings` and `flutter analyze`
without `--no-fatal-infos`, so it can pass where CI fails. Use the commands
above when checking whether a change is ready.

### Frontend code generation

Models and providers are generated (`json_serializable`, `freezed`,
`@riverpod`), and the generated `*.g.dart` / `*.freezed.dart` files are **not**
checked in. After changing an annotated Dart file — or after pulling changes
that touch one — regenerate them, otherwise `flutter analyze` reports missing
getters on fields that plainly exist in the source:

```bash
cd frontend
flutter pub run build_runner build --delete-conflicting-outputs
```

### Api Docs
You can access the API docs (scalar) `http://localhost:8080/docs` after starting the backend.

Or if you prefer Redoc `http://localhost:8080/redoc`.