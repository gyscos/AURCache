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

SRC = "/usr/bin/makechrootpkg"
DST = "/usr/local/bin/makechrootpkg"

# (description, anchor, replacement)
PATCHES = [
    (
        "download_sources: makepkg --verifysource",
        '\t\tmakepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
        '\t\taurcache-sandbox --allow-build-env -- makepkg --config="$copydir/etc/makepkg.conf" --verifysource -o',
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

    with open(DST, "w", encoding="utf-8") as out:
        out.write(text)

    # A shadow that is not executable is *silently skipped* by PATH lookup, so
    # the stock makechrootpkg runs and every build executes PKGBUILDs
    # unconfined, with nothing to indicate it. Set the mode, then verify it.
    os.chmod(DST, 0o755)
    mode = os.stat(DST).st_mode
    if not mode & stat.S_IXUSR or not os.access(DST, os.X_OK):
        print(
            f"patch-makechrootpkg: {DST} is not executable ({mode & 0o777:o}); "
            "PATH would silently fall through to the unpatched script.",
            file=sys.stderr,
        )
        return 1

    print(f"patch-makechrootpkg: confined {len(PATCHES)} PKGBUILD call sites in {DST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
