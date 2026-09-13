#!/usr/bin/env bash
# Install the rustup toolchain the packager stages cross-compile with.
#
# The packager always runs on amd64 (see worker.Dockerfile), so this installs
# the host x86_64 toolchain however many target architectures are in the build
# -- and that identical toolchain is the one thing every architecture races
# for. rustup instals each component with a `rename(2)` over a shared
# `<hash>.partial` file, so two architectures doing it at once fail with
# "could not rename 'downloaded' file ... No such file or directory" or
# "detected conflict: bin/cargo". The RUN that calls this therefore mounts the
# rustup home -- and the cargo home, whose bin/ collects the toolchain's shims
# -- with `sharing=locked`, serializing the race away.
#
# The per-architecture std is added later, by build-aurcache-packages.sh, under
# *shared* mounts: those components are named after the architecture, so racing
# architectures download different files and have nothing to clobber.
#
# Extra targets (the frontend's wasm32 std, on the hybrid image) as arguments.
set -eu
rustup default stable
for target in "$@"; do
    rustup target add "$target"
done