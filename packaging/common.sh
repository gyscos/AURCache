# Shared PKGBUILD helpers: how to build this repo's Rust binaries for $CARCH.
#
# Sourced by both PKGBUILDs rather than duplicated, so the two cannot disagree
# about targets, flags or where the output lands.

# The Rust target triple for the architecture makepkg is building for.
#
# Arch has no cross-compilation feature -- no `--target`, nothing in devtools --
# but `CARCH` is a plain shell variable makepkg sources and uses for the package
# name, so setting it and cross-compiling inside build() produces a correctly
# labelled package. That is a convention, not a supported mode, which is why it
# is written down here rather than assumed.
_aurcache_rust_target() {
    case "$CARCH" in
        x86_64) printf 'x86_64-unknown-linux-gnu' ;;
        aarch64) printf 'aarch64-unknown-linux-gnu' ;;
        *)
            printf 'aurcache: no Rust target known for CARCH=%s\n' "$CARCH" >&2
            return 1
            ;;
    esac
}

# Point cargo at a cross linker when building for a foreign architecture.
#
# Only aarch64 is covered: `aarch64-linux-gnu-gcc` and its glibc are in Arch's
# `extra` repository, while the armv7 equivalents are not packaged officially.
# An armv7h build would need the toolchain from the AUR, so it is refused here
# rather than failing later with a linker error nobody can place.
_aurcache_setup_cross() {
    local target=$1
    [[ $CARCH == "$(uname -m)" ]] && return 0

    case "$CARCH" in
        aarch64)
            if ! command -v aarch64-linux-gnu-gcc >/dev/null; then
                printf 'aurcache: cross-compiling to aarch64 needs aarch64-linux-gnu-gcc\n' >&2
                return 1
            fi
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
            export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
            export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
            ;;
        *)
            printf 'aurcache: no cross toolchain known for CARCH=%s\n' "$CARCH" >&2
            return 1
            ;;
    esac
}

# Where cargo leaves the binaries for $CARCH, relative to backend/.
#
# Its own function because makepkg runs package() in a separate fakeroot
# process that re-sources the PKGBUILD: anything build() assigned is gone by
# then, so the path has to be derivable rather than remembered.
_aurcache_release_dir() {
    local target
    target=$(_aurcache_rust_target) || return 1
    printf 'target/%s/release' "$target"
}

# Build the named workspace packages for $CARCH.
_aurcache_cargo_build() {
    local target
    target=$(_aurcache_rust_target) || return 1
    _aurcache_setup_cross "$target" || return 1

    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    # Strip through cargo rather than makepkg: makepkg would run the *host*
    # binutils over a foreign binary.
    export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C strip=symbols"

    cargo build --frozen --release --target "$target" "$@"
}

# Tests run the binaries, so they cannot run when those binaries are for
# another architecture. Skipped with a reason rather than silently.
_aurcache_cargo_check() {
    if [[ $CARCH != "$(uname -m)" ]]; then
        printf 'aurcache: skipping tests, %s binaries cannot run here\n' "$CARCH"
        return 0
    fi
    export RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=target
    cargo test --frozen --workspace
}
