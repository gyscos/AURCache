#!/bin/bash
# AURCache hybrid (compatibility) entrypoint: run the server and one embedded
# build worker in a single container.
#
# **Deprecated.** This exists so a pre-remote-worker compose file keeps building
# packages after an upgrade with no edits. The split `aurcache-server` +
# `aurcache-worker` images are the supported setup.
#
# Everything temporary about the compatibility path lives in this script — the
# shared secret, the legacy-mode inference, the supervision. Neither worker
# binary knows the other exists, so retiring this image retires the whole
# mechanism with it.
set -uo pipefail

log() { printf '[hybrid] %s\n' "$*" >&2; }

# ---------------------------------------------------------------------------
# Trust between the two processes.
#
# The embedded worker enrolls over the ordinary mTLS path — real CSR, real
# certificate, real worker row — and is auto-approved by a shared secret that
# never leaves this container. Regenerating it per boot is harmless: the token
# only ever approves a *pending* worker, so it cannot re-approve one that was
# revoked, nor disturb one already approved.
# ---------------------------------------------------------------------------
export AURCACHE_ENROLLMENT_TOKEN="${AURCACHE_ENROLLMENT_TOKEN:-$(head -c 32 /dev/urandom | base64 | tr -d '\n')}"
export AURCACHE_URL="${AURCACHE_URL:-https://localhost:8083}"
# The worker dials the server as `localhost`, so the listener's certificate has
# to be valid for it.
export AURCACHE_TLS_SANS="${AURCACHE_TLS_SANS:-localhost}"
# Auto-approval here is by token, not by shared volume; make sure a stale
# enrollment directory cannot also grant it.
unset AURCACHE_ENROLLMENT_DIR

# Name the embedded worker for what it is.
#
# Its default name is the hostname, which inside Docker is the container id --
# so every recreate produced a row called something like `b3f4a648533a`, and the
# Workers page read as a crowd of strangers rather than one worker that had come
# back. It is not a machine anyone chose to add; it is the one that ships in the
# box, and it should say so. Still overridable for a host running more than one.
export WORKER_NAME="${WORKER_NAME:-bundled}"

# ---------------------------------------------------------------------------
# Which builder.
#
# `BUILD_ARTIFACT_DIR` is not a hint — in the pre-worker code it *was* the
# definition of host build mode (`get_build_mode()` branched on exactly this).
# So a deployment that sets it was building in spawned containers against the
# host's Docker socket, and reproducing that is the faithful thing to do.
# Everything else gets the chroot worker, which is also what the old dind mode
# users end up with: they already run privileged, which is what it needs.
# ---------------------------------------------------------------------------
if [ -n "${BUILD_ARTIFACT_DIR:-}" ]; then
    WORKER_BIN=/usr/bin/aurcache-worker-docker
    WORKER_KIND="legacy container builder"
else
    WORKER_BIN=/usr/bin/aurcache-worker
    WORKER_KIND="devtools chroot builder"

    # devtools needs a writable /run and the ability to create mount
    # namespaces. Both come with `privileged`, which every documented
    # single-container setup already used.
    mountpoint -q /run || mount -t tmpfs tmpfs /run 2>/dev/null || true

    if ! unshare --mount --pid --fork true >/dev/null 2>&1; then
        log "ERROR: this container cannot create the mount/PID namespaces that"
        log "       chroot builds require, so no packages can be built here."
        log ""
        log "       Add 'privileged: true' to this service in your compose file"
        log "       and recreate it. If your setup instead mounts the Docker"
        log "       socket, set BUILD_ARTIFACT_DIR as it was before upgrading to"
        log "       use the legacy container builder."
        log ""
        log "       Starting the server alone; the web UI will be reachable and"
        log "       will report that no worker is available."
        WORKER_BIN=""
    fi
fi

# ---------------------------------------------------------------------------
# Supervision. Two processes, one PID 1: if either exits, bring the container
# down so the restart policy applies. A container that stays up while silently
# building nothing is the failure this whole image exists to prevent.
# ---------------------------------------------------------------------------
declare -a PIDS=()
shutdown() {
    trap - TERM INT
    for pid in "${PIDS[@]}"; do kill -TERM "$pid" 2>/dev/null; done
    wait
}
trap shutdown TERM INT

# Same delegation the split worker image does, minus the sudo: this entrypoint
# already runs as root. The embedded worker is dropped to `aurcache` below, and
# without this it cannot create the per-build cgroup that reports peak memory.
if [ -d /sys/fs/cgroup ]; then
    for f in /sys/fs/cgroup \
             /sys/fs/cgroup/cgroup.procs \
             /sys/fs/cgroup/cgroup.subtree_control; do
        chown aurcache:aurcache "$f" 2>/dev/null || true
    done
fi

log "starting AURCache server"
/usr/bin/aurcache &
PIDS+=($!)

if [ -n "$WORKER_BIN" ]; then
    log "starting embedded worker: $WORKER_KIND"
    log "NOTE: the hybrid image is deprecated. Migrate to the separate"
    log "      aurcache-server and aurcache-worker images when convenient."
    if [ -n "${BUILD_ARTIFACT_DIR:-}" ]; then
        # Legacy container builder: it drives the host's Docker socket, which is
        # root-owned, and it runs no build code locally — the spawned container
        # does. Staying root matches what the pre-worker server did, which is
        # the behaviour this path exists to reproduce.
        "$WORKER_BIN" run &
    else
        # The chroot worker runs as `aurcache`, not as the build user: builds are
        # started as `builder` via `makechrootpkg -U`, so the worker's mTLS
        # identity and credentials stay unreadable to PKGBUILD code by file
        # ownership alone.
        setpriv --reuid=aurcache --regid=aurcache --init-groups "$WORKER_BIN" run &
    fi
    PIDS+=($!)
fi

wait -n
status=$?
log "a supervised process exited (status $status); shutting down"
shutdown
exit "$status"
