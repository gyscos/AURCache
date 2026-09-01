#!/usr/bin/env python3
"""Confine the two places `makechrootpkg` executes a PKGBUILD on the worker.

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
    with open(DST, "w", encoding="utf-8") as out:
        out.write(text)

    # A copy that is not executable fails the build with a confusing "permission
    # denied" rather than anything naming this file. Set the mode, then verify.
    os.chmod(DST, 0o755)
    mode = os.stat(DST).st_mode
    if not mode & stat.S_IXUSR or not os.access(DST, os.X_OK):
        print(
            f"patch-makechrootpkg: {DST} is not executable ({mode & 0o777:o}).",
            file=sys.stderr,
        )
        return 1

    print(f"patch-makechrootpkg: confined {len(PATCHES)} PKGBUILD call sites in {DST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
