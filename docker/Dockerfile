ARG TARGET_ARCH=linux/amd64
ARG LATEST_COMMIT_SHA=dev
ARG FLUTTER_VERSION=3.44.1

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

# Must stay on the same Debian release as the runtime stage below: the default
# `rust:1.96.1` tag is trixie (glibc 2.41) and produces a binary the bookworm
# runtime (glibc 2.36) cannot load - it dies at startup with
# "GLIBC_2.38 not found".
FROM --platform=linux/amd64 rust:1.96.1-bookworm AS builder
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA}
# Install necessary tools and dependencies

WORKDIR /app

# Copy the Rust project files
COPY backend/ /app/
COPY --from=frontend_builder /app/build/web /app/aurcache-api/web

ARG TARGET_ARCH
ADD docker/build-rust.sh /root
RUN bash /root/build-rust.sh $TARGET_ARCH

FROM --platform=$TARGET_ARCH debian:bookworm-slim
# Podman/Docker is no longer used at runtime (remote workers build packages),
# so the server just needs a minimal glibc base. This must stay on the same
# Debian release as the builder stage above (see the note there). Runtime deps:
# ca-certificates (HTTPS to the AUR) and bash (the alpm-pkgbuild-bridge PKGBUILD
# parser is a bash script).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash \
    && rm -rf /var/lib/apt/lists/*
# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/target/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 docker/entrypoint.sh /entrypoint.sh

# alpm-pkgbuild-bridge parses a PKGBUILD by sourcing it, i.e. by executing
# attacker-supplied bash in the server process's container. It is installed out
# of PATH and reached only through a wrapper that confines it; see
# docker/bridge-wrapper.sh and backend/aurcache-sandbox.
ADD --chmod=755 https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads /usr/local/libexec/alpm-pkgbuild-bridge
COPY --from=builder --chmod=0755 /app/target/aurcache-sandbox /usr/local/bin/aurcache-sandbox
COPY --chmod=0755 docker/bridge-wrapper.sh /usr/local/bin/alpm-pkgbuild-bridge

WORKDIR /app
CMD ["/entrypoint.sh"]
