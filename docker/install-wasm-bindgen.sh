#!/usr/bin/env bash
# Build and install wasm-bindgen-cli at the version the frontend pins, so the
# CLI's schema matches the `wasm-bindgen` crate the frontend links against
# *exactly* -- the CLI refuses a wasm file emitted by any other version. The
# version is read out of the frontend's lockfile (the argument) rather than
# taken as whatever crates.io serves as latest.
#
# `--locked` does not do that, which is worth being explicit about because it
# reads as though it might: it pins the versions the CLI's *own* build resolves
# and says nothing about which release is selected. Unpinned, this worked until
# wasm-bindgen 0.2.128 was published and every build began failing with "rust
# Wasm file schema version: 0.2.127 / this binary schema version: 0.2.128".
#
# `rustup target add wasm32` is here rather than assumed installed: the server
# image needs it, and on the hybrid image it is already a no-op.
set -eu
lockfile="$1"
rustup target add wasm32-unknown-unknown
wb_version="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/[",]/, "", $3); print $3; exit }' "$lockfile")"
test -n "$wb_version"
cargo install wasm-bindgen-cli --locked --version "$wb_version"