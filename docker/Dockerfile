ARG LATEST_COMMIT_SHA=dev
# Must stay on the same Debian release as the runtime stage below: the default
# `rust:1.98.1` tag is trixie (glibc 2.41) and produces a binary the bookworm
# runtime (glibc 2.36) cannot load - it dies at startup with
# "GLIBC_2.38 not found".
FROM --platform=linux/amd64 rust:1.98.1-bookworm AS builder
ARG LATEST_COMMIT_SHA
ENV LATEST_COMMIT_SHA=${LATEST_COMMIT_SHA}
# Install necessary tools and dependencies

# The two trees keep the layout they have in the repository: the frontend
# reaches the shared crates as `../backend/...`, and `aurcache-api`'s build
# script reaches the frontend as `../../frontend-rs`. Flattening either breaks
# the other.
#
# The frontend is compiled by that build script, which the `static` feature in
# build-rust.sh switches on. There is no separate frontend stage, because one
# would have to be kept in step with the build script by hand — which is the
# coupling the script exists to remove.
COPY backend/ /app/backend/
COPY frontend-rs /app/frontend-rs/
WORKDIR /app/backend
# The CLI's version has to match the `wasm-bindgen` crate the frontend links
# against exactly, so it is read from the frontend's lockfile, which is already
# in the image by this point. `--locked` pins only the CLI's own dependencies
# and will still install whatever release is newest -- see hybrid.Dockerfile for
# the schema mismatch that produces.
RUN rustup target add wasm32-unknown-unknown \
    && wb_version="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/[",]/, "", $3); print $3; exit }' /app/frontend-rs/Cargo.lock)" \
    && test -n "$wb_version" \
    && cargo install wasm-bindgen-cli --locked --version "$wb_version"

# Which architecture to cross-compile for. `TARGETPLATFORM` is what buildx sets
# per target in a multi-platform build, so it is the default; `TARGET_ARCH`
# stays as an explicit override for the publish workflow, which builds one
# architecture at a time and names it itself. build-rust.sh accepts either
# spelling and rejects anything it does not recognise.
ARG TARGETPLATFORM
ARG TARGET_ARCH=
ADD docker/build-rust.sh /root
RUN bash /root/build-rust.sh "${TARGET_ARCH:-$TARGETPLATFORM}"

# No `--platform` here: buildx already builds this stage for the target, and
# pinning it to a build arg meant the runtime base and the binary above could
# be selected by two different values and disagree.
FROM debian:bookworm-slim
# Podman/Docker is no longer used at runtime (remote workers build packages),
# so the server just needs a minimal glibc base. This must stay on the same
# Debian release as the builder stage above (see the note there). Runtime deps:
# ca-certificates (HTTPS to the AUR) and bash (the alpm-pkgbuild-bridge PKGBUILD
# parser is a bash script).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash \
    && rm -rf /var/lib/apt/lists/*
# Copy the built binary from the previous stage
COPY --from=builder --chmod=0755 /app/backend/target/aurcache /usr/local/bin/aurcache
COPY --chmod=0755 docker/entrypoint.sh /entrypoint.sh

# alpm-pkgbuild-bridge parses a PKGBUILD by sourcing it, i.e. by executing
# attacker-supplied bash in the server process's container. It is installed out
# of PATH and reached only through a wrapper that confines it; see
# packaging/alpm-pkgbuild-bridge-wrapper and backend/aurcache-sandbox.
ADD --chmod=755 https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads /usr/local/libexec/alpm-pkgbuild-bridge
COPY --from=builder --chmod=0755 /app/backend/target/aurcache-sandbox /usr/local/bin/aurcache-sandbox
COPY --chmod=0755 packaging/alpm-pkgbuild-bridge-wrapper /usr/local/bin/alpm-pkgbuild-bridge
# The wrapper defaults to the native package's layout; this image keeps both
# under /usr/local, so point it at them rather than forking the script.
ENV AURCACHE_PKGBUILD_BRIDGE=/usr/local/libexec/alpm-pkgbuild-bridge \
    AURCACHE_SANDBOX=/usr/local/bin/aurcache-sandbox

WORKDIR /app
CMD ["/entrypoint.sh"]
