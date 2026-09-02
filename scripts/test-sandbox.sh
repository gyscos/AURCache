#!/usr/bin/env bash
# Build isolation regression test.
#
# A PKGBUILD is executed on the worker, outside any chroot, during source
# download — twice: by `makepkg --verifysource` and by makechrootpkg's direct
# `source PKGBUILD`. Unconfined, that lets one build write another's cached
# sources, the shared GnuPG keyring, and a concurrent job's PKGBUILD.
#
# This test asserts BOTH polarities, because an allow-list fails open: proving
# the denials only means something if the same fixture demonstrably escapes
# without the sandbox. A widened allow-list that reopens a hole turns the
# "unconfined" leg green in the confined run, and this test red.
#
# Usage: scripts/test-sandbox.sh [--keep]
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
[[ ${1:-} == --keep ]] && KEEP=1
IMAGE=aurcache-sandbox-test
WORKDIR=$(mktemp -d)
trap '(( KEEP )) || rm -rf "$WORKDIR"' EXIT

echo "==> building test image (Arch + devtools + sandbox)"
cargo build --release --manifest-path backend/Cargo.toml -p aurcache-sandbox >/dev/null
cp backend/target/release/aurcache-sandbox "$WORKDIR/"
cp docker/nspawn-wrapper.sh packaging/patch-makechrootpkg.py packaging/sandbox-protected "$WORKDIR/"
cp docker/testpkg/hostile-fixture/PKGBUILD "$WORKDIR/"

cat > "$WORKDIR/Dockerfile" <<'EOF'
FROM archlinux/archlinux:latest
RUN sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf \
 && pacman -Syu --noconfirm --needed base-devel devtools git sudo python \
 && pacman-key --init && pacman-key --populate && systemd-machine-id-setup \
 && useradd -m builder && echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
COPY aurcache-sandbox /usr/local/bin/aurcache-sandbox
COPY nspawn-wrapper.sh /usr/local/bin/systemd-nspawn
COPY patch-makechrootpkg.py /usr/local/bin/patch-makechrootpkg
# The same list the real images ship, so this test exercises the shipped
# policy rather than a copy of it that could drift.
COPY sandbox-protected /etc/aurcache/sandbox-protected
COPY PKGBUILD /fixture/PKGBUILD
RUN chmod +x /usr/local/bin/aurcache-sandbox /usr/local/bin/systemd-nspawn \
      /usr/local/bin/patch-makechrootpkg \
 && /usr/local/bin/patch-makechrootpkg \
 && mv /usr/lib/aurcache/bin/makechrootpkg /usr/local/bin/makechrootpkg.confined
COPY probe.sh /probe.sh
RUN chmod +x /probe.sh
EOF

cat > "$WORKDIR/probe.sh" <<'EOF'
#!/bin/bash
# $1 = "confined" | "unconfined"
set -u
mode=$1
mount -t tmpfs tmpfs /run 2>/dev/null || true

# The only difference between the legs is which makechrootpkg is on PATH:
# the patched copy, or the stock one from /usr/bin.
if [[ $mode == confined ]]; then
    cp /usr/local/bin/makechrootpkg.confined /usr/local/bin/makechrootpkg
else
    rm -f /usr/local/bin/makechrootpkg
fi

mkdir -p /srv && cd /srv && rm -rf upstream.git && git init -q --bare upstream.git
t=$(mktemp -d) && cd "$t" && git init -q . && git config user.email a@b && git config user.name a
echo hi > f.txt && git add . && git commit -qm init && git push -q /srv/upstream.git HEAD:master

rm -rf /shared /job && mkdir -p /shared/{gnupg,srcdest-other,work-other} /job/{srcdest,pkg} /chroot
echo legit > /shared/srcdest-other/victim-source

# Secrets the worker actually holds, in the location the shipped protected-path
# list names. Owned by the build user on purpose: the point is that Landlock
# denies the read even when file permissions would allow it.
mkdir -p /var/lib/aurcache-worker/secrets
echo SSH-KEY-MATERIAL > /var/lib/aurcache-worker/secrets/id_ed25519
echo MTLS-KEY-MATERIAL > /var/lib/aurcache-worker/identity.key
chown -R builder: /var/lib/aurcache-worker
cp /fixture/PKGBUILD /job/pkg/PKGBUILD
chown -R builder: /job /shared

[[ -d /chroot/root ]] || mkarchroot -C /etc/pacman.conf -M /etc/makepkg.conf /chroot/root base-devel git >/dev/null 2>&1

cd /job/pkg
sudo -u builder env SRCDEST=/job/srcdest makechrootpkg -c -r /chroot -l job1 -- --nocheck >/tmp/build.log 2>&1
echo "build_exit=$?"
echo "gnupg=$([ -e /shared/gnupg/EVIL_trustdb ] && echo written || echo blocked)"
echo "srcdest=$(cat /shared/srcdest-other/victim-source)"
echo "workdir=$([ -e /shared/work-other/EVIL_pkgbuild ] && echo written || echo blocked)"
# The payloads redirect into the (writable) package dir, so a non-empty file
# means the READ succeeded — this measures reads, not writes.
# Not a payload: a capability the sandbox must allow. Signature verification
# needs a temp file, and denying it breaks signed packages only.
echo "mktemp=$([ -s /job/pkg/mktemp-ok ] && echo works || echo broken)"
echo "sshkey=$([ -s /job/pkg/stolen-ssh ] && echo read || echo blocked)"
echo "mtlskey=$([ -s /job/pkg/stolen-mtls ] && echo read || echo blocked)"
EOF

docker build -q -t "$IMAGE" "$WORKDIR" >/dev/null

run() { docker run --rm --privileged --tmpfs /run "$IMAGE" /probe.sh "$1"; }

echo "==> leg 1/2: WITHOUT the sandbox (the fixture must escape)"
unconfined=$(run unconfined) || { echo "$unconfined"; exit 1; }
echo "$unconfined" | sed 's/^/    /'

echo "==> leg 2/2: WITH the sandbox (the fixture must be contained)"
confined=$(run confined) || { echo "$confined"; exit 1; }
echo "$confined" | sed 's/^/    /'

fail=0
check() { # name expected actual
    if [[ $3 == "$2" ]]; then echo "    ok   $1"; else echo "    FAIL $1: expected '$2', got '$3'"; fail=1; fi
}
get() { echo "$2" | grep "^$1=" | cut -d= -f2-; }

echo "==> assertions"
# Without the sandbox the fixture must actually escape, or the confined leg
# proves nothing.
check "unconfined: writes shared keyring"   written "$(get gnupg   "$unconfined")"
check "unconfined: writes other srcdest"    pwned   "$(get srcdest  "$unconfined")"
check "unconfined: writes other workdir"    written "$(get workdir  "$unconfined")"
check "unconfined: reads ssh key"          read    "$(get sshkey   "$unconfined")"
check "unconfined: reads mTLS identity"    read    "$(get mtlskey  "$unconfined")"
# With it, every write is denied and the build still succeeds.
check "confined:   keyring untouched"       blocked "$(get gnupg   "$confined")"
check "confined:   other srcdest intact"    legit   "$(get srcdest  "$confined")"
check "confined:   other workdir untouched" blocked "$(get workdir  "$confined")"
check "confined:   ssh key unreadable"     blocked "$(get sshkey   "$confined")"
check "confined:   mTLS identity unreadable" blocked "$(get mtlskey "$confined")"
check "confined:   mktemp still works"     works   "$(get mktemp    "$confined")"
check "confined:   build still succeeds"    0       "$(get build_exit "$confined")"

if (( fail )); then
    echo "==> FAILED"
    (( KEEP )) && echo "    test image kept as $IMAGE"
    exit 1
fi
# ---------------------------------------------------------------------------
# Phase 2: the server-side PKGBUILD parser.
#
# `alpm-pkgbuild-bridge` parses a PKGBUILD by sourcing it, in the process that
# owns the package database and the repository. Landlock fits this case best:
# no mounting is involved, and the set to protect is a single directory.
# ---------------------------------------------------------------------------
echo
echo "==> phase 2: server-side PKGBUILD parser"
cp packaging/alpm-pkgbuild-bridge-wrapper "$WORKDIR/"
curl -fsSL "https://gitlab.archlinux.org/archlinux/alpm/alpm-pkgbuild-bridge/-/raw/main/alpm-pkgbuild-bridge.sh?ref_type=heads" \
    -o "$WORKDIR/bridge.sh"

cat > "$WORKDIR/Dockerfile.server" <<'EOF'
# Mirrors the server image's base.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends bash ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY aurcache-sandbox /usr/local/bin/aurcache-sandbox
COPY bridge.sh /usr/local/libexec/alpm-pkgbuild-bridge
COPY alpm-pkgbuild-bridge-wrapper /usr/local/bin/alpm-pkgbuild-bridge
ENV AURCACHE_PKGBUILD_BRIDGE=/usr/local/libexec/alpm-pkgbuild-bridge \
    AURCACHE_SANDBOX=/usr/local/bin/aurcache-sandbox
COPY server-probe.sh /probe.sh
RUN chmod +x /usr/local/bin/aurcache-sandbox /usr/local/libexec/alpm-pkgbuild-bridge \
      /usr/local/bin/alpm-pkgbuild-bridge /probe.sh
EOF

cat > "$WORKDIR/server-probe.sh" <<'EOF'
#!/bin/bash
# $1 = "confined" | "unconfined"
set -u
mode=$1
# The state the server owns and a PKGBUILD must not reach.
mkdir -p /app/db /app/repo
echo "PACKAGE-DATABASE" > /app/db/aurcache.db
echo "BUILT-PACKAGE"    > /app/repo/hello-1-1-x86_64.pkg.tar.zst

work=$(mktemp -d)
cat > "$work/PKGBUILD" <<'PKG'
pkgname=probe
pkgver=1
pkgrel=1
arch=('x86_64')
$(cat /app/db/aurcache.db >> /tmp/out-db 2>/dev/null)
$(cat /app/repo/hello-1-1-x86_64.pkg.tar.zst >> /tmp/out-pkg 2>/dev/null)
$(echo "corrupted" > /app/repo/hello-1-1-x86_64.pkg.tar.zst 2>/dev/null)
$(printf '%s' "$DB_PWD" >> /tmp/out-env 2>/dev/null)
package() { :; }
PKG

rm -f /tmp/out-db /tmp/out-pkg /tmp/out-env
if [[ $mode == confined ]]; then
    bridge=/usr/local/bin/alpm-pkgbuild-bridge
else
    bridge=/usr/local/libexec/alpm-pkgbuild-bridge
fi
# /tmp is where payloads land; the parse's own dir is $work.
out=$("$bridge" "$work/PKGBUILD" 2>/dev/null)
echo "parse_ok=$(echo "$out" | grep -c '^VAR GLOBAL')"
echo "db_read=$([ -s /tmp/out-db ] && echo read || echo blocked)"
echo "pkg_read=$([ -s /tmp/out-pkg ] && echo read || echo blocked)"
echo "pkg_corrupted=$(grep -q corrupted /app/repo/hello-1-1-x86_64.pkg.tar.zst && echo yes || echo no)"
echo "secret_from_env=$([ -s /tmp/out-env ] && echo leaked || echo blocked)"
EOF

docker build -q -t "$IMAGE-server" -f "$WORKDIR/Dockerfile.server" "$WORKDIR" >/dev/null
srun() { docker run --rm -e DB_PWD=SUPER-SECRET-DB-PASSWORD "$IMAGE-server" /probe.sh "$1"; }

echo "==> leg 1/2: WITHOUT the wrapper"
s_unconfined=$(srun unconfined); echo "$s_unconfined" | sed 's/^/    /'
echo "==> leg 2/2: WITH the wrapper"
s_confined=$(srun confined); echo "$s_confined" | sed 's/^/    /'

echo "==> assertions"
check "unconfined: reads the database"      read     "$(get db_read        "$s_unconfined")"
check "unconfined: reads a built package"   read     "$(get pkg_read       "$s_unconfined")"
check "unconfined: corrupts a built package" yes     "$(get pkg_corrupted  "$s_unconfined")"
check "unconfined: reads DB_PWD from env"   leaked   "$(get secret_from_env "$s_unconfined")"
check "confined:   database unreadable"     blocked  "$(get db_read        "$s_confined")"
check "confined:   built package unreadable" blocked "$(get pkg_read       "$s_confined")"
check "confined:   built package intact"    no       "$(get pkg_corrupted  "$s_confined")"
check "confined:   DB_PWD not in env"       blocked  "$(get secret_from_env "$s_confined")"
check "confined:   parse still works"       5        "$(get parse_ok       "$s_confined")"

if (( fail )); then
    echo "==> FAILED"
    (( KEEP )) && echo "    test images kept as $IMAGE, $IMAGE-server"
    exit 1
fi
echo "==> PASSED"
