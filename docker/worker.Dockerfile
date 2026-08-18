# syntax=docker/dockerfile:1
#
# AURCache remote build worker image.
#
# Multi-stage: compile the `aurcache-worker` binary from source (stage 1),
# then assemble a minimal Arch image with `devtools` and an unprivileged
# `builder` user (stage 2). Run privileged — `arch-nspawn`/`mkarchroot` need
# mount + unshare.

########## Stage 1: build the worker binary ##########
FROM --platform=linux/amd64 rust:1.97 AS builder
WORKDIR /app
# The worker only depends on aurcache-types, but cargo needs the whole
# workspace present to resolve it.
COPY backend/ /app/
RUN cargo build --release -p aurcache-worker \
    && cp target/release/aurcache-worker /usr/local/bin/aurcache-worker

########## Stage 2: runtime image ##########
FROM archlinux/archlinux:latest AS final

# devtools provides mkarchroot / makechrootpkg / arch-nspawn.
RUN --mount=type=cache,target=/var/cache/pacman/pkg \
    sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
    && pacman -Syu --noconfirm --needed \
        base-devel devtools sudo git fakeroot \
    && pacman-key --init \
    && pacman-key --populate archlinux \
    && systemd-machine-id-setup

# Unprivileged build user with passwordless sudo. `makechrootpkg` must not run
# as root (makepkg refuses); sudo sets SUDO_USER=builder so makepkg drops to it.
RUN useradd --create-home --shell /bin/bash builder \
    && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder \
    && chmod 0440 /etc/sudoers.d/builder

# Data / chroot / cache locations, writable by the build user.
ENV WORKER_DATA_DIR=/var/lib/aurcache-worker \
    WORKER_CHROOT_DIR=/var/lib/aurcache-worker/chroot \
    WORKER_CACHE_DIR=/var/cache/aurcache-worker
RUN mkdir -p "$WORKER_DATA_DIR" "$WORKER_CHROOT_DIR" "$WORKER_CACHE_DIR" \
    && chown -R builder:builder "$WORKER_DATA_DIR" "$WORKER_CACHE_DIR"

COPY --from=builder /usr/local/bin/aurcache-worker /usr/local/bin/aurcache-worker
# Wrapper so devtools' systemd-nspawn works without a systemd manager (see script).
COPY --chmod=0755 docker/nspawn-wrapper.sh /usr/local/bin/systemd-nspawn

USER builder
WORKDIR /home/builder

# Default: enroll and poll for jobs. Override the command for `build-once`.
ENTRYPOINT ["/usr/local/bin/aurcache-worker"]
CMD ["run"]
