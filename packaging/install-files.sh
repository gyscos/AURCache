#!/bin/bash
# Install AURCache's packaging files into $pkgdir.
#
# Shared by the server and worker PKGBUILDs. They differ in which binaries and
# units they carry; everything else about what a package *is* -- the users, the
# directories, the modes -- lives here, so the two cannot drift on ownership of
# the paths they share. The container images install these same packages, which
# is what stops an image being a second, hand-maintained copy of the host
# contract.
#
# Usage: install-files.sh server|worker <pkgdir> <repo-root> <binary-dir>
set -euo pipefail

readonly ROLE=$1
readonly PKGDIR=$2
readonly ROOT=$3
readonly BINDIR=$4

readonly PKG="$ROOT/packaging"

# Users and directories are declared for both roles, not just the worker: the
# two packages are installable together and would otherwise disagree about who
# owns /var/lib/aurcache. systemd-sysusers and tmpfiles are idempotent, and
# pacman is content for two packages to ship the same file only if it is
# byte-identical -- which it is, being the same file.
install -Dm644 "$PKG/aurcache.sysusers" "$PKGDIR/usr/lib/sysusers.d/aurcache.conf"
install -Dm644 "$PKG/aurcache.tmpfiles" "$PKGDIR/usr/lib/tmpfiles.d/aurcache.conf"

case "$ROLE" in
server)
    install -Dm755 "$BINDIR/aurcache" "$PKGDIR/usr/bin/aurcache"
    install -Dm644 "$PKG/aurcache.service" \
        "$PKGDIR/usr/lib/systemd/system/aurcache.service"
    install -Dm644 "$PKG/server.env" "$PKGDIR/etc/aurcache/server.env"

    # The server parses a PKGBUILD by sourcing it, which runs attacker-supplied
    # bash in the process holding the database credentials. `alpm-srcinfo`
    # resolves the bridge through PATH, so the confining wrapper is installed
    # under the same name in a directory the unit puts *first* -- shadowing
    # /usr/bin/alpm-pkgbuild-bridge the way the worker shadows makechrootpkg.
    # Installing the bridge without this wrapper would parse unconfined.
    install -Dm755 "$BINDIR/aurcache-sandbox" "$PKGDIR/usr/bin/aurcache-sandbox"
    install -Dm755 "$PKG/alpm-pkgbuild-bridge-wrapper" \
        "$PKGDIR/usr/lib/aurcache/bin/alpm-pkgbuild-bridge"
    ;;
worker)
    install -Dm755 "$BINDIR/aurcache-worker" "$PKGDIR/usr/bin/aurcache-worker"
    # Confines the two places makechrootpkg executes a PKGBUILD outside the
    # chroot. Without it every build dies at source download.
    install -Dm755 "$BINDIR/aurcache-sandbox" "$PKGDIR/usr/bin/aurcache-sandbox"

    install -Dm644 "$PKG/aurcache-worker.service" \
        "$PKGDIR/usr/lib/systemd/system/aurcache-worker.service"

    # The patcher and its wrapper, plus the hook that re-runs them when devtools
    # changes. The derived copy is deliberately not shipped: it has to come from
    # the devtools installed on the machine, or it would run against another
    # release's libraries.
    install -Dm644 "$PKG/patch-makechrootpkg.py" \
        "$PKGDIR/usr/lib/aurcache/patch-makechrootpkg.py"
    install -Dm755 "$PKG/aurcache-patch-makechrootpkg" \
        "$PKGDIR/usr/lib/aurcache/aurcache-patch-makechrootpkg"
    install -Dm644 "$PKG/aurcache-makechrootpkg.hook" \
        "$PKGDIR/usr/share/libalpm/hooks/aurcache-makechrootpkg.hook"

    # Paths a PKGBUILD must never read; see the file for why it is not an
    # environment variable.
    install -Dm644 "$PKG/sandbox-protected" "$PKGDIR/etc/aurcache/sandbox-protected"
    install -Dm644 "$PKG/worker.env" "$PKGDIR/etc/aurcache/worker.env"
    install -Dm440 "$PKG/aurcache-worker.sudoers" \
        "$PKGDIR/etc/sudoers.d/aurcache-worker"
    ;;
*)
    echo "install-files.sh: unknown role '$ROLE'" >&2
    exit 1
    ;;
esac
