#!/usr/bin/env bash
# Build the server binaries for the target architecture.
#
# Two binaries are produced:
#   aurcache         - the server itself.
#   aurcache-sandbox - confines `alpm-pkgbuild-bridge`, which parses a PKGBUILD
#                      by sourcing it, i.e. by executing it in the server's
#                      container. See backend/aurcache-sandbox.
#
# Both are moved to predictable paths under target/ for the Dockerfile to copy.
set -o pipefail
set -o errexit
set -o nounset
set -o verbose
set -o xtrace

TARGET_ARCH="${1:-ab}"

# `--features static` applies to aurcache only; the sandbox has no such feature,
# so the two are built separately rather than in one invocation.
if [ "$TARGET_ARCH" == "linux/arm64/v8" ]; then
    RUST_TARGET=aarch64-unknown-linux-gnu
    rustup target add "$RUST_TARGET"
    apt update -y && apt install -y gcc-aarch64-linux-gnu
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    cargo build --release --target="$RUST_TARGET" --features static
    cargo build --release --target="$RUST_TARGET" -p aurcache-sandbox
    mv "target/$RUST_TARGET/release/aurcache" target/aurcache
    mv "target/$RUST_TARGET/release/aurcache-sandbox" target/aurcache-sandbox
else
    cargo build --release --features static
    cargo build --release -p aurcache-sandbox
    mv target/release/aurcache target/aurcache
    mv target/release/aurcache-sandbox target/aurcache-sandbox
fi
