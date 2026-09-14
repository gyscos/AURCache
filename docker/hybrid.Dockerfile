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
ENV PATH="/home/packager/bin:/home/packager/.cargo/bin:${PATH}"
# The scripts need no source, so they are copied ahead of the tree and a code
# change does not invalidate them.
COPY --chmod=0755 docker/install-rust-toolchain.sh docker/build-aurcache-packages.sh docker/install-wasm-bindgen.sh /usr/local/bin/

# The lockfile alone, ahead of the source, so that the expensive `cargo install`
# below is invalidated when the pinned version changes and not by a code edit.
COPY frontend-rs/Cargo.lock /tmp/frontend-Cargo.lock
# wasm-bindgen emits the frontend's JS glue and is not in Arch's repositories,
# so it is built rather than installed. See install-wasm-bindgen.sh for why its
# version comes from the lockfile rather than crates.io. The cache mount
# carries the registry and the installed wasm-bindgen-cli, so the install is
# skipped rather than recompiled once the worker or a previous hybrid build has
# run. `uid`/`gid` solve the reason this used to be impossible: buildkit
# creates the mount as root, leaving cargo -- running as `packager` -- unable to
# write `~/.cargo/.crates.toml`; naming the owner makes the home writable and
# the layer caches on its own again. The toolchain comes from
# install-rust-toolchain.sh, which serializes rustup across the architectures
# building at once and reinstalls it if buildkit evicted the mount; the wasm32
# std is added there too. The binary itself is copied out of the
# mount into the layer (`~/.cargo` is a cache mount, which buildkit may evict
# independently of the layer cache -- the same trap server.Dockerfile copies
# itself out of); `~/.cargo/bin` stays on PATH for the compiles that run under
# `cargo`, and `~/bin` is where the durable copy lives.
RUN --mount=type=cache,target=/home/packager/.cargo,id=cargo-downloads-packager,uid=1000,gid=1000,mode=0700 \
    --mount=type=cache,target=/home/packager/.rustup,id=rustup-downloads-packager,uid=1000,gid=1000,mode=0700 \
    install-rust-toolchain.sh wasm32-unknown-unknown \
    && install-wasm-bindgen.sh /tmp/frontend-Cargo.lock \
    && install -Dm755 /home/packager/.cargo/bin/wasm-bindgen /home/packager/bin/wasm-bindgen

COPY --chown=packager . /src
# Build all four packages: the worker-docker package is a wrapper image of its
# own, and the server needs the same sandbox binary whose paths pacman refuses
# to double-check across two packages. See build-aurcache-packages.sh for the
# build itself.
#
# The cache mounts stop `prepare()`'s `cargo fetch` and the per-architecture
# `rustup target add` from redownloading what the worker image already fetched.
# Sharing them between architectures is safe because rustup is only reached
# through install-rust-toolchain.sh's lock, and cargo locks its own cache.
RUN --mount=type=cache,target=/home/packager/.cargo,id=cargo-downloads-packager,uid=1000,gid=1000,mode=0700 \
    --mount=type=cache,target=/home/packager/.rustup,id=rustup-downloads-packager,uid=1000,gid=1000,mode=0700 \
    build-aurcache-packages.sh aurcache-sandbox aurcache-server aurcache-worker aurcache-worker-docker

########## Stage 1c: export the built packages to the host ##########
# The image installs the packages and discards them (`rm -rf /tmp/pkg` below),
# but the build *made* the packages, and a host that wants them for a native
# install should not re-make them with build-packages.sh. This stage gives the
# finished archives back: nothing in the image flow references it, so it costs
# the normal build nothing, and `scripts/build-images.sh --packages-dir` runs one
# extra `buildx build --target export-pkgs` that replays the now-cached
# (host-arch) packager stage and copies /pkg out with `type=local` output.
FROM scratch AS export-pkgs
COPY --from=packager /pkg/*.pkg.tar.zst /

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
# The sync database is refreshed *here* rather than inherited from the layer
# above, even though that layer has just done the same thing. `pacman -U`
# resolves this package's dependencies against whatever database the image
# carries, and the layer that populated it is cached across builds while the
# repositories behind it are not: Arch Linux ARM rotates a superseded package
# out of its pool within weeks. So a code change invalidates this layer, the
# cached database still names python-dulwich-1.1.0-1, and every mirror answers
# 404 for a file none of them has kept -- which reads as the mirrors being
# broken rather than as the database being stale. Re-syncing in the same layer
# as the install is what keeps the two from drifting apart.
RUN --mount=type=cache,target=/var/cache/pacman/pkg,id=pacman-runtime-${TARGETPLATFORM} \
    pacman -Syu --noconfirm --needed \
    && pacman -U --noconfirm /tmp/pkg/*.pkg.tar.zst \
    && rm -rf /tmp/pkg \
    && systemd-sysusers \
    && systemd-tmpfiles --create

ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CACHE_DIR=/var/cache/aurcache-worker

# Wrapper so devtools' systemd-nspawn works without a systemd manager.
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
COPY --chmod=0755 docker/hybrid-entrypoint.sh /usr/local/bin/hybrid-entrypoint

# State that must outlive the container.
#
#   /app/data                 the internal CA that signs every worker's
#                             certificate.
#   /var/lib/aurcache-worker  the embedded worker's identity, and its base chroot.
#   /var/cache/aurcache-worker  its package and source caches.
#
# `/app/data` was missing here, and its absence was worse than it looks: losing
# the CA invalidates the certificate of *every* worker that ever enrolled, so a
# recreate orphaned the whole fleet and not just the embedded worker. The two
# directories below were declared without the one that makes them useful.
#
# Declaring them means `docker compose up` carries them across a recreate even
# for a compose file that mounts none of them. That is not universal, though:
# an orchestrator that deletes and recreates the container rather than updating
# it in place -- TrueNAS's app system among them -- starts each deployment with
# fresh anonymous volumes, and the embedded worker then enrolls as a new machine
# every single time. A deployment that updates regularly should mount named
# volumes here explicitly rather than trusting the anonymous ones to survive.
VOLUME ["/app/data", "/var/lib/aurcache-worker", "/var/cache/aurcache-worker"]

WORKDIR /app
CMD ["/usr/local/bin/hybrid-entrypoint"]
