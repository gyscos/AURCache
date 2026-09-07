#!/usr/bin/env python3
"""Patch `makechrootpkg` for the worker: confinement, and one nspawn default.

Mostly the first. The last patch in the list is unrelated to confinement and
says so; it opts out of a systemd-nspawn default that is about to change.

A PKGBUILD is bash. Sourcing one runs it. `makechrootpkg` does that twice
*outside* the chroot, as the build user:

  1. `download_sources()` runs `makepkg --verifysource`, and makepkg sources the
     PKGBUILD to learn what to fetch;
  2. it runs `source PKGBUILD` directly to read `pkgbase`/`pkgname`.

Unconfined, either one lets a build write every other build's files — another
package's cached sources, the shared GnuPG keyring, a concurrent job's PKGBUILD.
Checksums are no defence: they are declared by the PKGBUILD, so rewriting a
neighbour's PKGBUILD rewrites its checksums too.

Both sites are wrapped in `aurcache-sandbox`, which applies a Landlock policy
allowing writes only to that build's own directories. The chroot build itself is
untouched: Landlock refuses `mount(2)` outright, so nothing confined this way
could run `arch-nspawn`.

Writes a shadowed copy to /usr/local/bin, which precedes /usr/bin in both PATH
and sudo's `secure_path`; the packaged script is left alone.

Every patch MUST apply exactly once. If devtools moves a call site this fails
the image build rather than silently shipping a worker that executes PKGBUILDs
unconfined — the reason a patch is preferred here over shadowing `makepkg` on
PATH, whose failure mode would be invisible.
"""

import os
import stat
import sys

SRC = os.environ.get("MAKECHROOTPKG_SRC", "/usr/bin/makechrootpkg")
# A location AURCache owns, named by the worker as an absolute path.
#
# Not `/usr/local/bin`: that belongs to the administrator rather than to a
# package, and a copy there shadows `makechrootpkg` for *every* user on the
# host, so someone building something unrelated by hand silently gets this
# sandboxed variant. It was chosen originally because it is on sudo's
# `secure_path` -- the worker runs devtools through sudo, which replaces PATH --
# and naming the file absolutely removes that constraint entirely.
DST = os.environ.get("MAKECHROOTPKG_DST", "/usr/lib/aurcache/bin/makechrootpkg")

# (description, anchor, replacement)
PATCHES = [
    (
        "download_sources: makepkg --verifysource",
        '\t\tmakepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
        '\t\taurcache-sandbox --allow-build-env -- makepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
    ),
    (
        "download_sources: keep temp files inside the job's own directory",
        # makepkg's signature verification does `statusfile=$(mktemp)`, which
        # lands in /tmp — outside the sandbox's allow-list, so every package
        # with signed sources fails with "mktemp: Permission denied" followed
        # by a confusing "No such file or directory" from gpg's status parser.
        # Pointing TMPDIR at $WORKDIR keeps those files in a directory the job
        # already owns, so /tmp stays unwritable.
        '\t\tenv SRCDEST="$SRCDEST" BUILDDIR="$WORKDIR" \\',
        '\t\tenv SRCDEST="$SRCDEST" BUILDDIR="$WORKDIR" TMPDIR="$WORKDIR" \\',
    ),
    (
        "direct `source PKGBUILD` for pkgbase/pkgname",
        '} < <(sudo -u "$makepkg_user" bash -c \'',
        '} < <(sudo -u "$makepkg_user" aurcache-sandbox --allow-build-env -- bash -c \'',
    ),
    (
        # Not a confinement patch. systemd-nspawn currently permits every socket
        # address family and warns, in every build log, that a future version
        # will default to AF_INET, AF_INET6 and AF_UNIX only. Builds would lose
        # AF_NETLINK -- which glibc uses for `getaddrinfo`'s AI_ADDRCONFIG, so
        # name resolution is implicated -- along with AF_ALG and AF_PACKET.
        # Whatever an arbitrary PKGBUILD needs, it is not for this to guess, so
        # opt out explicitly: `--restrict-address-families=` with an empty
        # argument is the documented way to keep today's behaviour, and it
        # silences the notice as a side effect.
        #
        # A shell function rather than five edits: makechrootpkg calls
        # `arch-nspawn` unqualified in five places, and a function of that name
        # takes precedence over the PATH lookup in all of them. The flag has to
        # land straight after the working directory, since everything after it
        # is the command to run inside the container.
        #
        # Guarded on the version because the option only exists from systemd
        # 261. Both supported targets are Arch and roll forward, but a worker
        # mid-upgrade should build rather than fail on an unknown option.
        "arch-nspawn wrapper opting out of address-family filtering",
        "bindmounts_ro=()",
        """_aurcache_nspawn_version=$(systemd-nspawn --version 2>/dev/null \\
\t| awk 'NR==1 {print $2 + 0; exit}')
if [[ -n ${_aurcache_nspawn_version:-} ]] && (( _aurcache_nspawn_version >= 261 )); then
\tarch-nspawn() {
\t\tlocal _aurcache_dir=$1
\t\tshift
\t\tcommand arch-nspawn "$_aurcache_dir" --restrict-address-families= "$@"
\t}
fi

bindmounts_ro=()""",
    ),
]


def main() -> int:
    text = open(SRC, encoding="utf-8").read()

    for description, anchor, replacement in PATCHES:
        found = text.count(anchor)
        if found != 1:
            print(
                f"patch-makechrootpkg: expected exactly one call site for "
                f"{description!r} in {SRC}, found {found}.\n"
                f"  devtools changed this call site; re-derive the patch before\n"
                f"  shipping, or builds will execute PKGBUILDs unconfined.",
                file=sys.stderr,
            )
            return 1
        text = text.replace(anchor, replacement, 1)

    os.makedirs(os.path.dirname(DST), exist_ok=True)

    # Written beside the target and renamed over it, never opened for writing in
    # place. `open(DST, "w")` truncates, and bash reads a script incrementally as
    # it runs -- so re-deriving this while a build was in flight rewrote the file
    # under the `makechrootpkg` executing it, mid-build. A rename swaps the
    # directory entry and leaves the running copy holding its own inode, which
    # is also what lets the package be upgraded while builds are running.
    tmp = f"{DST}.new"
    with open(tmp, "w", encoding="utf-8") as out:
        out.write(text)

    # A copy that is not executable fails the build with a confusing "permission
    # denied" rather than anything naming this file. Set the mode, then verify.
    os.chmod(tmp, 0o755)
    os.replace(tmp, DST)
    mode = os.stat(DST).st_mode
    if not mode & stat.S_IXUSR or not os.access(DST, os.X_OK):
        print(
            f"patch-makechrootpkg: {DST} is not executable ({mode & 0o777:o}).",
            file=sys.stderr,
        )
        return 1

    print(f"patch-makechrootpkg: applied {len(PATCHES)} patches to {DST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
