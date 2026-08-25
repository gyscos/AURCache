#!/bin/bash
# Start the build credential's ssh-agent and publish its socket to builds.
#
# The key is owned by the worker's user and is listed in the sandbox's protected
# paths, so the build user can neither read it nor open it. That is deliberate:
# a PKGBUILD executes on the worker, outside any chroot, while sources are being
# fetched. But the fetch is exactly what needs the key.
#
# An agent resolves that: it holds the key in the worker's own process and hands
# out signatures over a socket. A build can *use* the credential without ever
# being able to read it, so nothing it captures outlives the build.
# `makechrootpkg` already forwards SSH_AUTH_SOCK
# (`--preserve-env=GNUPGHOME,SSH_AUTH_SOCK`), so nothing else has to change.
#
# Host restrictions (`ssh-add -h`) are deliberately not applied: sources may
# legitimately live on any host, so there is no set to pin. Agent hijacking
# during a build is therefore possible and accepted; the property we keep is
# that the key itself cannot be stolen.
#
# Sourced by the worker entrypoints, which then export SSH_AUTH_SOCK.
set -u

agent_socket_dir=${AURCACHE_AGENT_DIR:-/run/aurcache}
key=${WORKER_DATA_DIR:-/var/lib/aurcache-worker}/secrets/id_ed25519

start_build_agent() {
    [ -r "$key" ] || { echo "[worker] no build credential; skipping ssh-agent" >&2; return 0; }

    # The socket lives in a directory the build group can traverse; the key does
    # not. `sudo` is used because /run is root-owned tmpfs and the worker is not.
    sudo mkdir -p "$agent_socket_dir" 2>/dev/null || mkdir -p "$agent_socket_dir" || return 0
    sudo chown "$(id -un):aurbuild" "$agent_socket_dir" 2>/dev/null || true
    sudo chmod 0750 "$agent_socket_dir" 2>/dev/null || true

    local sock="$agent_socket_dir/agent.sock"
    rm -f "$sock"
    eval "$(ssh-agent -a "$sock" 2>/dev/null)" >/dev/null || {
        echo "[worker] ssh-agent failed to start; authenticated sources will fail" >&2
        return 0
    }
    # Group-readable so builds can authenticate through it. The key itself stays
    # unreadable to them.
    chgrp aurbuild "$sock" 2>/dev/null || true
    chmod 0660 "$sock" 2>/dev/null || true

    if ssh-add "$key" >/dev/null 2>&1; then
        echo "[worker] build credential loaded into ssh-agent at $sock" >&2
        export SSH_AUTH_SOCK="$sock"
    else
        echo "[worker] could not load build credential into ssh-agent" >&2
    fi
}
