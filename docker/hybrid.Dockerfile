# syntax=docker/dockerfile:1
#
# AURCache hybrid (compatibility) image — server + an embedded build worker.
#
# **Deprecated.** This image exists so that deployments predating the
# remote-worker architecture keep working after an upgrade without editing
# their compose file. New deployments should run the split images:
# `aurcache-server` plus one or more `aurcache-worker` containers, which can be
# scaled and placed independently. See the Build Workers documentation.
#
# The base is Arch because the embedded chroot worker needs `devtools`. Run
# privileged (as the old single-container setups already did) so `arch-nspawn`
# / `mkarchroot` can create the mounts and namespaces a chroot build requires.
#
# amd64, arm64 and armv7. armv7's cross toolchain is AUR-only, so it is built
# in a stage of its own that the other two never reach.

ARG LATEST_COMMIT_SHA=dev

########## Stage 1: build every package ##########
# One stage, on the native build host, cross-compiling for the target — cargo
# under qemu turns minutes into a great deal longer. Everything this image runs
# is built here as a real package, so the image installs what a host installs
# rather than re-declaring the same users, directories and modes by hand.
#
# Pinned to amd64 rather than $BUILDPLATFORM: the official Arch image is
# x86_64-only, so there is no native packager base on an aarch64 builder, and
# Arch Linux ARM packages no x86_64 cross toolchain either. Building the images
# therefore wants an amd64 host; on anything else this stage runs emulated and
# slowly rather than failing.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS packager-base
# The cache id carries the platform: a buildkit cache mount is keyed on its
# target path, so without this the amd64 packager's x86_64 downloads land in
# the very cache the arm64 and armv7 runtimes read from. That surfaces as
# "could not find package in cache" and as signatures from the Arch Linux ARM
# build system being rejected -- neither of which points at a shared cache.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-packager \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    # rustup rather than the `rust` package: Arch ships std for the host
    # architecture only, so a cross build fails with "can't find crate for
    # `std`". rustup fetches precompiled std per target, wasm32 included.
    && pacman -Syu --noconfirm --needed base-devel rustup git \
        aarch64-linux-gnu-gcc \
    # makepkg refuses to run as root, and rightly: a PKGBUILD is a shell script.
    && useradd --create-home packager \
    # The armv7 toolchain stage installs each link of the chain as it builds
    # it, so the unprivileged build user needs pacman without a password.
    # Nothing else here installs anything -- makepkg runs --nodeps.
    && echo 'packager ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/packager \
    && install -d -o packager /pkg

########## Stage 1b: the cross toolchain for the target ##########
# amd64 and arm64 need nothing more; armv7h's toolchain is AUR-only and is
# built here, before any source is copied, so Docker's layer cache keeps it
# across code changes. See docker/worker.Dockerfile for the full reasoning.
FROM packager-base AS toolchain-amd64
FROM packager-base AS toolchain-arm64

FROM packager-base AS toolchain-armv7
# Point this at a pacman repository holding prebuilt cross-toolchain packages
# and the bootstrap is skipped for whatever it carries -- an AURCache instance
# serves nicely. Anything the repository lacks is still built from the AUR, so
# an unset or unreachable value costs correctness nothing, only time.
ARG AURCACHE_TOOLCHAIN_REPO=
ENV AURCACHE_TOOLCHAIN_REPO=${AURCACHE_TOOLCHAIN_REPO}
# The repository's section name, which decides the database file pacman asks
# for. Defaults to what an AURCache instance serves.
ARG AURCACHE_TOOLCHAIN_REPO_NAME=repo
ENV AURCACHE_TOOLCHAIN_REPO_NAME=${AURCACHE_TOOLCHAIN_REPO_NAME}
COPY --chmod=0755 packaging/build-cross-toolchain.sh /usr/local/bin/build-cross-toolchain
USER packager
RUN build-cross-toolchain armv7h
USER root

ARG TARGETARCH
ARG TARGETVARIANT
FROM toolchain-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS packager
ARG TARGETARCH
ARG TARGETVARIANT
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA}

USER packager
# wasm-bindgen emits the frontend's JS glue and is not in Arch's repositories,
# so it is built rather than installed. Its version must match the
# `wasm-bindgen` crate in frontend-rs/Cargo.lock, which `--locked` guarantees
# by using that crate's own lockfile.
ENV PATH="/home/packager/.cargo/bin:${PATH}"
RUN rustup default stable && rustup target add wasm32-unknown-unknown
# No cache mount here: buildkit creates the mount's parent as root, leaving
# cargo unable to write ~/.cargo/.crates.toml. The layer caches on its own.
RUN cargo install wasm-bindgen-cli --locked

COPY --chown=packager . /src
# `--skipinteg` because the tarball is this tree rather than a release, and
# `--nodeps` because the build needs nothing from the target architecture:
# dependencies are recorded in the package and resolved where it is installed.
RUN set -eux; \
    case "${TARGETARCH}${TARGETVARIANT:-}" in \
      amd64) CARCH=x86_64 ;; \
      arm64) CARCH=aarch64 ;; \
      armv7) CARCH=armv7h ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}${TARGETVARIANT:-}"; exit 1 ;; \
    esac; \
    export CARCH; \
    # From common.sh, so it cannot drift from the triple cargo is asked for.
    rustup target add "$(. /src/packaging/common.sh && _aurcache_rust_target)"; \
    for p in aurcache-sandbox aurcache-server aurcache-worker aurcache-worker-docker; do \
      cd "/src/packaging/$p"; \
      /src/packaging/make-source-tarball.sh /src "$p" 0.5.0 .; \
      makepkg --nodeps --skipinteg --noconfirm --nocheck; \
      cp ./*.pkg.tar.zst /pkg/; \
    done

########## Stage 2: per-arch Arch Linux runtime ##########
# Official Arch is x86_64-only; Arch Linux ARM covers arm64.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS runtime-amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS runtime-arm64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS runtime-armv7

ARG TARGETARCH
ARG TARGETVARIANT
FROM runtime-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final
# Interpolated into the pacman cache id below.
ARG TARGETPLATFORM

# DisableSandbox: pacman 7's Landlock-based download sandbox cannot initialise
# inside an unprivileged/nested container, which makes every `pacman -Sy` abort.
# It only affects pacman's own download isolation, not the per-build chroot.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
    && pacman-key --init \
    && pacman-key --populate \
    && systemd-machine-id-setup

# Everything this image runs, installed as packages: the four binaries, the two
# users and their group membership, the directories and their modes, the
# sudoers entry, the patched makechrootpkg, and the wrapper that confines the
# server's PKGBUILD parser. pacman pulls devtools and alpm-pkgbuild-bridge as
# ordinary dependencies.
#
# This is the point of packaging: the image stops being a second,
# hand-maintained copy of the host contract that can drift from the documented
# one. systemd-sysusers / systemd-tmpfiles run explicitly as well as through
# pacman's hooks — an image with no systemd manager is exactly where a hook
# that quietly did not run would go unnoticed until a build failed for want of
# a directory. Both are idempotent.
COPY --from=packager /pkg/*.pkg.tar.zst /tmp/pkg/
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    pacman -U --noconfirm /tmp/pkg/*.pkg.tar.zst \
    && rm -rf /tmp/pkg \
    && systemd-sysusers \
    && systemd-tmpfiles --create

ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CACHE_DIR=/var/cache/aurcache-worker

# Wrapper so devtools' systemd-nspawn works without a systemd manager.
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
COPY --chmod=0755 docker/ssh-agent-setup.sh /usr/local/bin/aurcache-ssh-agent-setup
COPY --chmod=0755 docker/hybrid-entrypoint.sh /usr/local/bin/hybrid-entrypoint

# The embedded worker's identity and its expensive base chroot live here.
# Declaring them as volumes means Compose carries them across a container
# recreate even for deployments whose compose file predates the worker and
# therefore mounts neither — so upgrading does not orphan the worker's identity
# (which would enroll a new worker every restart) or rebuild the base chroot.
VOLUME ["/var/lib/aurcache-worker", "/var/cache/aurcache-worker"]

WORKDIR /app
CMD ["/usr/local/bin/hybrid-entrypoint"]
