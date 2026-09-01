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
# The base is Arch rather than Debian because the embedded chroot worker needs
# `devtools`; the server itself is base-agnostic. Run privileged (as the old
# single-container setups already did) so `arch-nspawn` / `mkarchroot` can
# create the mounts and namespaces a chroot build requires.

ARG FLUTTER_VERSION=3.44.1
ARG LATEST_COMMIT_SHA=dev

########## Stage 1: web frontend ##########
FROM --platform=linux/amd64 debian:bookworm-slim AS frontend_builder
ARG FLUTTER_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
      git curl xz-utils unzip ca-certificates && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://storage.googleapis.com/flutter_infra_release/releases/stable/linux/flutter_linux_${FLUTTER_VERSION}-stable.tar.xz" \
      | tar -xJ -C /opt
ENV PATH="/opt/flutter/bin:${PATH}"
# Flutter refuses to run as root against a git dir it considers "dubious ownership"
RUN git config --global --add safe.directory /opt/flutter \
      && flutter config --no-analytics && flutter precache --web
WORKDIR /app
COPY frontend /app
COPY backend/aurcache/Cargo.toml /app
RUN flutter pub get
RUN flutter pub run build_runner build --delete-conflicting-outputs
RUN flutter build web --release --wasm --dart-define APP_VERSION=$(grep '^version' Cargo.toml | sed -E 's/version *= *"([^"]+)"/\1/')

########## Stage 2: all three binaries in one cargo pass ##########
# Cross-compiled on the build host (no emulation), exactly as the worker image
# does; Arch's glibc is newer than the builder's, so the binaries load fine.
FROM --platform=$BUILDPLATFORM rust:1.97 AS builder
ARG TARGETARCH
ARG TARGETVARIANT
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA}
WORKDIR /app
COPY backend/ /app/
COPY --from=frontend_builder /app/build/web /app/aurcache-api/web
RUN set -eux; \
    case "${TARGETARCH}${TARGETVARIANT:-}" in \
      amd64) RUST_TARGET=x86_64-unknown-linux-gnu; PKGS="" ;; \
      arm64) RUST_TARGET=aarch64-unknown-linux-gnu; PKGS="gcc-aarch64-linux-gnu" ;; \
      armv7) RUST_TARGET=armv7-unknown-linux-gnueabihf; PKGS="gcc-arm-linux-gnueabihf" ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}${TARGETVARIANT:-}"; exit 1 ;; \
    esac; \
    if [ -n "$PKGS" ]; then apt-get update && apt-get install -y --no-install-recommends $PKGS; fi; \
    rustup target add "$RUST_TARGET"; \
    case "$RUST_TARGET" in \
      aarch64-*) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
                        CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
                        AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar ;; \
      armv7-*)   export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc \
                        CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
                        AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar ;; \
    esac; \
    cargo build --release --target "$RUST_TARGET" \
        -p aurcache -p aurcache-worker -p aurcache-worker-docker -p aurcache-sandbox; \
    mkdir -p /out; \
    cp "target/$RUST_TARGET/release/aurcache" \
       "target/$RUST_TARGET/release/aurcache-worker" \
       "target/$RUST_TARGET/release/aurcache-worker-docker" \
       "target/$RUST_TARGET/release/aurcache-sandbox" /out/

########## Stage 3: per-arch Arch Linux runtime ##########
FROM --platform=linux/amd64 archlinux/archlinux:latest AS runtime-amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS runtime-arm64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS runtime-armv7

ARG TARGETARCH
ARG TARGETVARIANT
FROM runtime-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final

# devtools provides mkarchroot / makechrootpkg / arch-nspawn.
#
# DisableSandbox: pacman 7's Landlock-based download sandbox cannot initialise
# inside an unprivileged/nested container, which makes every `pacman -Sy` abort.
# It only affects pacman's own download isolation, not the per-build chroot.
RUN --mount=type=cache,target=/var/cache/pacman/pkg \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
        base-devel devtools sudo git fakeroot openssh ca-certificates bash \
    && pacman-key --init \
    && pacman-key --populate \
    && systemd-machine-id-setup

# Two unprivileged users, deliberately separate:
#
#   aurcache - runs the worker process. Owns the mTLS identity and the build
#              credentials, and is the only user that can read them.
#   builder  - runs each package build. `makechrootpkg -U builder` names it
#              explicitly, so a build never inherits the worker's user through
#              SUDO_USER and never reaches the worker's secrets by file
#              permissions.
#
# They share a group so both can use the caches: builds write SRCDEST, and the
# worker garbage-collects it. sudo is what lets the unprivileged worker start a
# build as another user, so the worker itself never needs to be root.
# `aurbuild` is builder's *primary* group, not a supplementary one. That
# matters: makechrootpkg carries only the build user's primary uid/gid into the
# chroot, so a supplementary group does not exist in there and group-write on
# the bind-mounted caches silently fails with "no write permission for $SRCDEST"
# — from inside the chroot, after the host-side checks have already passed.
RUN groupadd aurbuild \
    && useradd --create-home --shell /bin/bash --gid aurbuild builder \
    && useradd --create-home --shell /bin/bash --groups aurbuild aurcache \
    && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder \
    && echo 'aurcache ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/aurcache \
    && chmod 0440 /etc/sudoers.d/builder /etc/sudoers.d/aurcache

ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CACHE_DIR=/var/cache/aurcache-worker
# Ownership is the second half of the uid split, and it is what protects the
# worker's secrets even if the Landlock policy is never applied:
#
#   data dir   - owned by aurcache, traversable so builds can reach work/,
#                but identity and secrets inside are aurcache-only.
#   work/      - owned by *builder*, shared to aurcache by group.
#   cache dir  - likewise.
#
# The build dirs are owned by the build user rather than the worker because
# makechrootpkg copies only the build user's primary uid/gid into the chroot:
# supplementary groups do not exist in the chroot's /etc/group, so group-based
# write access silently fails inside it with "no write permission for $SRCDEST".
# Ownership by uid is the only form that survives. The worker reaches these
# through the shared group, which it does not need inside any chroot.
RUN mkdir -p "$WORKER_DATA_DIR/work" "$WORKER_DATA_DIR/secrets" \
        "$WORKER_CACHE_DIR" \
    && chown -R aurcache:aurcache "$WORKER_DATA_DIR" \
    && chmod 0755 "$WORKER_DATA_DIR" \
    && chmod 0700 "$WORKER_DATA_DIR/secrets" \
    && chown builder:aurbuild "$WORKER_DATA_DIR/work" "$WORKER_CACHE_DIR" \
    && chmod 2775 "$WORKER_DATA_DIR/work" "$WORKER_CACHE_DIR"

COPY --from=builder /out/aurcache /usr/local/bin/aurcache
COPY --from=builder /out/aurcache-worker /usr/local/bin/aurcache-worker
COPY --from=builder /out/aurcache-worker-docker /usr/local/bin/aurcache-worker-docker
COPY --from=builder /out/aurcache-sandbox /usr/local/bin/aurcache-sandbox
# Wrapper so devtools' systemd-nspawn works without a systemd manager.
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
# Confine the two places makechrootpkg executes a PKGBUILD on the worker,
# outside the chroot. See backend/aurcache-sandbox.
COPY --chmod=0755 packaging/patch-makechrootpkg.py /usr/local/bin/patch-makechrootpkg
# Paths a PKGBUILD must never read; see the file for why it is not an env var.
COPY packaging/sandbox-protected /etc/aurcache/sandbox-protected
COPY --chmod=0755 docker/ssh-agent-setup.sh /usr/local/bin/aurcache-ssh-agent-setup
RUN pacman -S --noconfirm --needed python && /usr/local/bin/patch-makechrootpkg
COPY --chmod=0755 docker/hybrid-entrypoint.sh /usr/local/bin/hybrid-entrypoint

# add alpm-pkgbuild-bridge (the server's PKGBUILD parser)
ADD --chmod=755 https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads /usr/local/bin/alpm-pkgbuild-bridge

# The embedded worker's identity and its expensive base chroot live here.
# Declaring them as volumes means Compose carries them across a container
# recreate even for deployments whose compose file predates the worker and
# therefore mounts neither — so upgrading does not orphan the worker's identity
# (which would enroll a new worker every restart) or rebuild the base chroot.
VOLUME ["/var/lib/aurcache-worker", "/var/cache/aurcache-worker"]

WORKDIR /app
CMD ["/usr/local/bin/hybrid-entrypoint"]
