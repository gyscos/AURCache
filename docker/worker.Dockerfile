# syntax=docker/dockerfile:1
#
# AURCache remote build worker image (multi-arch).
#
# The worker runs one long-lived process per architecture and builds every
# package in a `devtools` chroot. Foreign architectures are supported by running
# this same image emulated via qemu-user + binfmt (see docker-compose.yaml), so
# the build path is uniform across arches — an aarch64 worker is just this image
# run with `--platform linux/arm64`.
#
# Stage 1 cross-compiles the `aurcache-worker` binary on the native build host
# (fast — no cargo emulation), targeting the requested arch. Stage 2 assembles a
# minimal Arch base for that arch with `devtools`. Run privileged: arch-nspawn /
# mkarchroot need mount + unshare.

########## Stage 1: cross-compile the worker binary ##########
# Runs natively on the builder's platform; TARGETARCH selects the Rust target.
FROM --platform=$BUILDPLATFORM rust:1.97 AS builder
ARG TARGETARCH
ARG TARGETVARIANT
WORKDIR /app
COPY backend/ /app/
# Map the Docker target arch to a Rust target + cross toolchain, then build.
# Setting CC/AR for the target lets C-backed crates (e.g. ring) cross-compile.
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
    cargo build --release --target "$RUST_TARGET" -p aurcache-worker -p aurcache-sandbox; \
    cp "target/$RUST_TARGET/release/aurcache-worker" \
       "target/$RUST_TARGET/release/aurcache-sandbox" /usr/local/bin/

########## Stage 2: per-arch Arch Linux runtime base ##########
# Official Arch is x86_64-only; Arch Linux ARM images cover arm64 / armv7.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS runtime-amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS runtime-arm64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS runtime-armv7

# Select the base matching the target arch (buildkit resolves the alias).
ARG TARGETARCH
ARG TARGETVARIANT
FROM runtime-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final

# devtools provides mkarchroot / makechrootpkg / arch-nspawn.
#
# DisableSandbox: pacman 7's Landlock-based download sandbox cannot initialise
# inside an unprivileged/nested container (no Landlock access here), which makes
# every `pacman -Sy` abort. Disabling it is required for pacman to run at all in
# this image; it only affects pacman's own download isolation, not the per-build
# chroot isolation (each build still runs in its own `makechrootpkg` chroot).
RUN --mount=type=cache,target=/var/cache/pacman/pkg \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
        base-devel devtools sudo git fakeroot openssh \
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

# Data / chroot / cache locations, writable by the build user.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CHROOT_DIR=/var/lib/aurcache-worker/chroot \
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
        "$WORKER_CHROOT_DIR" "$WORKER_CACHE_DIR" \
    && chown -R aurcache:aurcache "$WORKER_DATA_DIR" \
    && chmod 0755 "$WORKER_DATA_DIR" \
    && chmod 0700 "$WORKER_DATA_DIR/secrets" \
    && chown builder:aurbuild "$WORKER_DATA_DIR/work" "$WORKER_CACHE_DIR" \
    && chmod 2775 "$WORKER_DATA_DIR/work" "$WORKER_CACHE_DIR"

COPY --from=builder /usr/local/bin/aurcache-worker /usr/local/bin/aurcache-worker
# Confines the two places makechrootpkg executes a PKGBUILD outside the chroot.
# Without it those calls fail with "env: 'aurcache-sandbox': No such file", and
# every build dies at source download.
COPY --from=builder /usr/local/bin/aurcache-sandbox /usr/local/bin/aurcache-sandbox
# Wrapper so devtools' systemd-nspawn works without a systemd manager (see script).
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
# Confine the two places makechrootpkg executes a PKGBUILD on the worker,
# outside the chroot. See backend/aurcache-sandbox.
COPY --chmod=0755 packaging/patch-makechrootpkg.py /usr/local/bin/patch-makechrootpkg
# Paths a PKGBUILD must never read; see the file for why it is not an env var.
COPY packaging/sandbox-protected /etc/aurcache/sandbox-protected
COPY --chmod=0755 docker/ssh-agent-setup.sh /usr/local/bin/aurcache-ssh-agent-setup
RUN pacman -S --noconfirm --needed python && /usr/local/bin/patch-makechrootpkg
# Entrypoint fixes shared-enroll-volume ownership before dropping to the worker.
COPY --chmod=0755 docker/worker-entrypoint.sh /usr/local/bin/worker-entrypoint

USER aurcache
WORKDIR /home/aurcache

# Default: enroll and poll for jobs. Override the command for `build-once`.
ENTRYPOINT ["/usr/local/bin/worker-entrypoint"]
CMD ["run"]
