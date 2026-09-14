#!/usr/bin/env bash
# Cross-compile AURCache packages for the build's target architecture and copy
# the archives into /pkg. Shared by the worker and hybrid images; which
# packages to build is the caller's argument (the server image is Debian and
# does not build packages at all).
#
# Arch has no cross-compilation feature -- no `--target`, nothing in devtools --
# but `CARCH` is a plain shell variable makepkg sources and uses for the package
# name, so setting it and cross-compiling inside build() produces a correctly
# labelled package from one PKGBUILD. That is a convention, not a supported
# mode, which is why it is written down here rather than assumed.
#
# Every architecture runs this at once over the same rustup home, so rustup is
# only ever reached through install-rust-toolchain.sh, which serializes it and
# restores a toolchain the cache mount lost. The compiles themselves share the
# cargo home freely: cargo locks its own caches.
set -euxo pipefail

case "${TARGETARCH}${TARGETVARIANT:-}" in
    amd64) CARCH=x86_64 ;;
    arm64) CARCH=aarch64 ;;
    armv7) CARCH=armv7h ;;
    *)
        echo "unsupported TARGETARCH=${TARGETARCH}${TARGETVARIANT:-}" >&2
        exit 1
        ;;
esac
export CARCH

# The Rust target triple comes from common.sh, which the PKGBUILDs source
# too, so this cannot drift from the one cargo is asked for.
. /src/packaging/common.sh
install-rust-toolchain.sh "$(_aurcache_rust_target)"

for package in "$@"; do
    cd "/src/packaging/$package"
    /src/packaging/make-source-tarball.sh /src "$package" 0.5.0 .
    # `--skipinteg` because the tarball is this tree rather than a release, and
    # `--nodeps` because the *build* needs nothing from the target architecture:
    # dependencies are recorded in the package and resolved where it is
    # installed.
    makepkg --nodeps --skipinteg --noconfirm --nocheck
    cp ./*.pkg.tar.zst /pkg/
done