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
# Stage 1 cross-compiles and packages on the native build host (fast — no cargo
# emulation). Stage 2 installs that package into a minimal Arch base for the
# target arch, so the image and a native install are the same thing. Run
# privileged: arch-nspawn / mkarchroot need mount + unshare.
#
# amd64, arm64 and armv7. armv7's cross toolchain is not in Arch's official
# repositories, so it is built from the AUR in a stage of its own -- see the
# toolchain stages below for why that costs nothing on the other two.

########## Stage 1: cross-compile and package ##########
# Runs natively on the builder and cross-compiles, which is the whole reason
# this is not simply built in the target-arch image: cargo under qemu turns
# minutes into a great deal longer.
#
# Arch has no cross-compilation feature, but `CARCH` is a plain shell variable
# makepkg uses for the package name, and aarch64's toolchain is in `extra`. So
# exporting CARCH and cross-compiling inside build() yields a correctly labelled
# package from one PKGBUILD -- the same one a native install uses.
# Pinned to amd64 rather than $BUILDPLATFORM: the official Arch image is
# x86_64-only (ARM Linux uses the lopsided/ images below), so there is no native
# packager base on an aarch64 builder -- and Arch Linux ARM packages no x86_64
# cross toolchain either, so an ARM host could not produce the amd64 package
# even if there were. Building the images therefore needs an amd64 host; on
# anything else this stage runs emulated and slowly rather than failing, which
# is worth knowing before wondering why a build takes an hour.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS packager-base
# The cache id carries the platform: a buildkit cache mount is keyed on its
# target path, so without this the amd64 packager's x86_64 downloads land in
# the very cache the arm64 and armv7 runtimes read from. That surfaces as
# "could not find package in cache" and as signatures from the Arch Linux ARM
# build system being rejected -- neither of which points at a shared cache.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-packager \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    # rustup rather than the `rust` package: Arch ships std for the host
    # architecture only, so `cargo build --target aarch64-...` fails with
    # "can't find crate for `std`". rustup fetches precompiled std per target.
    && pacman -Syu --noconfirm --needed base-devel rustup git \
        aarch64-linux-gnu-gcc \
    # makepkg refuses to run as root, and rightly: a PKGBUILD is a shell script.
    && useradd --create-home packager \
    && echo 'packager ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/packager \
    # The build stage runs unprivileged, so its output directory has to exist
    # and be writable before the drop to `packager`.
    && install -d -o packager /pkg

########## Stage 1b: the cross toolchain for the target ##########
# amd64 and arm64 need nothing beyond what the base already has: the build is
# native for one and `aarch64-linux-gnu-gcc` from `extra` covers the other.
FROM packager-base AS toolchain-amd64
FROM packager-base AS toolchain-arm64

# armv7h's cross toolchain is not in any official repository, so it is built
# from the AUR -- seven packages, gcc three times, the better part of an hour.
#
# Deliberately *before* any source is copied in. Docker caches this layer and
# only rebuilds it when the script itself changes, but only if no earlier layer
# has been invalidated: put it after `COPY . /src` and every code change pays
# the hour again. CI runners start cold, so the workflow feeds buildx a cache.
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

COPY --chown=packager . /src
USER packager
# `--skipinteg` because the tarball is this tree rather than a release, and
# `--nodeps` because the *build* needs nothing from the target architecture:
# dependencies are recorded in the package and resolved where it is installed.
RUN set -eux; \
    case "${TARGETARCH}${TARGETVARIANT:-}" in \
      amd64) CARCH=x86_64 ;; \
      arm64) CARCH=aarch64 ;; \
      armv7) CARCH=armv7h ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}${TARGETVARIANT:-}"; exit 1 ;; \
    esac; \
    export CARCH; \
    # The triple comes from common.sh, which the PKGBUILD uses too, so the
    # target rustup installs cannot drift from the one cargo is asked for.
    rustup default stable; \
    rustup target add "$(. /src/packaging/common.sh && _aurcache_rust_target)"; \
    # Both packages: aurcache-worker depends on aurcache-sandbox, which is its
    # own package because the server needs the same binary and pacman refuses
    # two packages shipping one path.
    for p in aurcache-sandbox aurcache-worker; do \
      cd "/src/packaging/$p"; \
      /src/packaging/make-source-tarball.sh /src "$p" 0.5.0 .; \
      makepkg --nodeps --skipinteg --noconfirm --nocheck; \
      cp ./*.pkg.tar.zst /pkg/; \
    done

########## Stage 2: per-arch Arch Linux runtime base ##########
# Official Arch is x86_64-only; Arch Linux ARM images cover arm64 / armv7.
FROM --platform=linux/amd64 archlinux/archlinux:latest AS runtime-amd64
FROM --platform=linux/arm64 lopsided/archlinux:latest AS runtime-arm64
FROM --platform=linux/arm/v7 lopsided/archlinux-arm32v7:latest AS runtime-armv7

# Select the base matching the target arch (buildkit resolves the alias).
ARG TARGETARCH
ARG TARGETVARIANT

########## Stage 2b: the worker image ##########
FROM runtime-${TARGETARCH}${TARGETVARIANT:+${TARGETVARIANT}} AS final
# Interpolated into the pacman cache id below.
ARG TARGETPLATFORM

# devtools provides mkarchroot / makechrootpkg / arch-nspawn.
#
# DisableSandbox: pacman 7's Landlock-based download sandbox cannot initialise
# inside an unprivileged/nested container (no Landlock access here), which makes
# every `pacman -Sy` abort. Disabling it is required for pacman to run at all in
# this image; it only affects pacman's own download isolation, not the per-build
# chroot isolation (each build still runs in its own `makechrootpkg` chroot).
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
    && pacman-key --init \
    && pacman-key --populate \
    && systemd-machine-id-setup

# Everything the worker needs on a host -- the binaries, the two users, the
# directories and their modes, the sudoers entry, the sandbox policy, and the
# patched makechrootpkg -- installs from the same package a native install uses.
#
# That is the point of packaging first: the image stops being a second,
# hand-maintained copy of the host contract that can drift from the documented
# one. `pacman -U` pulls devtools and python as ordinary dependencies, and
# systemd's own hooks apply the sysusers and tmpfiles declarations.
# `systemd-sysusers` / `systemd-tmpfiles` are run explicitly as well as by
# pacman's own hooks: an image with no systemd manager is exactly where a hook
# that quietly did not run would go unnoticed until a build failed for want of
# a directory. Both are idempotent.
COPY --from=packager /pkg/*.pkg.tar.zst /tmp/pkg/
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    pacman -U --noconfirm /tmp/pkg/*.pkg.tar.zst \
    && rm -rf /tmp/pkg \
    && systemd-sysusers \
    && systemd-tmpfiles --create

# The same locations the package's tmpfiles declaration creates. A container
# has no systemd unit to carry them, so they are set here instead.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CHROOT_DIR=/var/lib/aurcache-worker/chroot \
    WORKER_CACHE_DIR=/var/cache/aurcache-worker

# Wrapper so devtools' systemd-nspawn works without a systemd manager (see
# script). Container-only: a real host has a manager and needs none of this.
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
COPY --chmod=0755 docker/ssh-agent-setup.sh /usr/local/bin/aurcache-ssh-agent-setup
# Entrypoint fixes shared-enroll-volume ownership before dropping to the worker.
COPY --chmod=0755 docker/worker-entrypoint.sh /usr/local/bin/worker-entrypoint

USER aurcache
WORKDIR /var/lib/aurcache-worker

# Default: enroll and poll for jobs. Override the command for `build-once`.
ENTRYPOINT ["/usr/local/bin/worker-entrypoint"]
CMD ["run"]
