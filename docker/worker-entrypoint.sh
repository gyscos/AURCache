#!/bin/bash
set -e
# AURCache worker entrypoint.
#
# Bundled auto-approval works by the worker dropping its CSR into a shared
# `enroll` volume that the server reads. The worker runs as the unprivileged
# `builder` user, but a Docker named volume is initialised root-owned by
# whichever container mounts it first (often the server, read-only). Use the
# build user's passwordless sudo to take ownership of just that directory so the
# CSR can be written. Best-effort: if there is no enroll dir (token/preapproved
# enrollment, or a remote worker), this is a no-op.
if [ -n "${AURCACHE_ENROLLMENT_DIR:-}" ]; then
    sudo mkdir -p "${AURCACHE_ENROLLMENT_DIR}" 2>/dev/null || true
    sudo chown "$(id -u):$(id -g)" "${AURCACHE_ENROLLMENT_DIR}" 2>/dev/null || true
fi

# Hold the build credential in an agent rather than handing builds the key
# file, which they cannot read (see docker/ssh-agent-setup.sh).
# shellcheck source=/dev/null
. /usr/local/bin/aurcache-ssh-agent-setup
start_build_agent

exec /usr/bin/aurcache-worker "$@"
