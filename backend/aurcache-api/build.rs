//! Builds the web frontend and puts it where `RustEmbed` will find it.
//!
//! `#[derive(RustEmbed)] #[folder = "web"]` bakes the bundle in at compile
//! time, so a stale `web/` means a server quietly serving an old UI. Doing it
//! from here rather than from a script means the dependency is expressed rather
//! than remembered: the `rerun-if-changed` lines below make cargo rebuild the
//! server whenever the frontend changes, and nobody has to notice.
//!
//! **This does nothing unless the `static` feature is on.** A build script
//! cannot be feature-gated away — it always runs — so it checks the feature
//! itself and returns. Building the server without `static` therefore costs
//! nothing here and needs no wasm toolchain, which is what someone working only
//! on the backend gets.
//!
//! The nested cargo invocation is safe because `frontend-rs` is its own
//! workspace with its own target directory: the two builds never contend for
//! the same lock. The cargo-set environment is cleared for the child all the
//! same, since inheriting `CARGO_TARGET_DIR` would undo exactly that.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Cheap and unconditional: without it, adding the feature later would not
    // rerun this script and the first `static` build would embed nothing.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_STATIC");

    if std::env::var_os("CARGO_FEATURE_STATIC").is_none() {
        return;
    }

    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let frontend = manifest
        .join("../../frontend-rs")
        .canonicalize()
        .expect("frontend-rs sits beside the backend workspace");
    let web = manifest.join("web");

    // What makes a rebuild automatic. Cargo walks a directory given here, so
    // the sources are covered by naming `src` rather than every file in it.
    for path in ["src", "Cargo.toml", "Cargo.lock", "index.html"] {
        println!("cargo:rerun-if-changed={}", frontend.join(path).display());
    }

    build_wasm(&frontend);
    bindgen(&frontend);
    install(&frontend.join("dist"), &web);
}

/// Compile the frontend crate to wasm.
fn build_wasm(frontend: &Path) {
    run(
        Command::new(cargo()).current_dir(frontend).args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ]),
        "cargo build (frontend)",
        "Is the wasm32-unknown-unknown target installed? \
         `rustup target add wasm32-unknown-unknown`",
    );
}

/// Turn the wasm into something a browser can load.
fn bindgen(frontend: &Path) {
    run(
        Command::new("wasm-bindgen").current_dir(frontend).args([
            "--target",
            "web",
            "--out-dir",
            "dist",
            "--no-typescript",
            "target/wasm32-unknown-unknown/release/aurcache-frontend.wasm",
        ]),
        "wasm-bindgen",
        "Is wasm-bindgen-cli installed? `cargo install wasm-bindgen-cli`",
    );
}

/// Copy the bundle, and the page that loads it, into the embed folder.
///
/// The folder is emptied first: a file dropped from the frontend would
/// otherwise stay embedded indefinitely.
fn install(dist: &Path, web: &Path) {
    let _ = std::fs::remove_dir_all(web);
    std::fs::create_dir_all(web).expect("create web/");

    std::fs::copy(
        dist.parent().expect("dist has a parent").join("index.html"),
        web.join("index.html"),
    )
    .expect("copy index.html");
    copy_tree(dist, web);
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dist/") {
        let entry = entry.expect("read dist/ entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&target).expect("create dir");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// The cargo that is running this build, so the child uses the same toolchain.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Run a command, failing the build with something legible if it does not work.
fn run(command: &mut Command, what: &str, hint: &str) {
    // Cargo exports its own build's settings, and a child cargo would take
    // them as its own — `CARGO_TARGET_DIR` most of all, which would point the
    // frontend at the server's target directory and deadlock on its lock.
    for key in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC",
        "RUSTDOC",
        "CARGO_MAKEFLAGS",
    ] {
        command.env_remove(key);
    }

    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("{what} failed with {status}. {hint}"),
        Err(e) => panic!("could not run {what}: {e}. {hint}"),
    }
}
