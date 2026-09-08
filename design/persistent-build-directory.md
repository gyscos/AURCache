# Persistent build directories

Plan for keeping a package's build tree between builds, so a long compilation
is not repeated from scratch, and so a failure late in a build is recoverable.

Status: **implemented**, except where noted under "What this does not
solve". The reclaim policy below differs from what was first designed; the
reason is recorded there.

---

## The failure that prompted it

`unreal-engine` compiled for 3h47m, reported `BUILD SUCCESSFUL`, and then died
in `package()`:

```
install: cannot stat '../unreal-engine.sh': No such file or directory
==> Removing chroot copy [/var/lib/aurcache-chroot/job-645-3001993]...done
```

Every byte of those four hours was inside the chroot copy, and the copy was
deleted the moment the build ended. Nothing was recoverable. A retry would have
started again from an empty tree -- and would have hit the same `package()` bug,
because that failure is a PKGBUILD portability defect, not a transient one.

The salvage that *should* have been available is `makepkg --repackage`, which
skips download, extract, `prepare()` and `build()` and runs only `package()`
against the existing `$srcdir`. Minutes rather than hours. It needs the build
tree to still exist.

## What persists today, and what does not

| | keyed by | persists | holds |
|---|---|---|---|
| `SRCDEST` | pkgbase | **yes** | what makepkg *downloads* |
| `BUILDDIR` | -- | **no** | what makepkg *extracts and builds* |

`SRCDEST` is already per-package and persistent
(`<cache>/srcdest/<pkgbase>`, 21 packages and 4.2 GB on the reference
worker). It is bind-mounted into the chroot and survives everything. It is
not shared between packages, deliberately: a single pooled `SRCDEST` was
considered and rejected, because concurrent chain builds race on the same
partial downloads.

`BUILDDIR` is `/build` inside the chroot copy, so it dies with the copy.

For most packages this is barely noticeable -- the sources are in `SRCDEST` and
extraction is local. For `unreal-engine` it is the whole cost, because the
UnrealEngine tree is **not declared in `source=()`**. It is cloned inside
`prepare()`:

```bash
git clone --depth=1 --branch=${pkgver}-release git@github.com:EpicGames/UnrealEngine "${pkgname}"
```

so makepkg's download cache never sees it. `srcdest/unreal-engine` holds only
the 1.59 GB toolchain tarball, while the build needed 129 GB. Everything else is
fetched again on every attempt.

That PKGBUILD is written expecting the tree to survive:

```bash
# Download Unreal Engine source or update if the folder exists
if [[ ! -d "${pkgname}" ]]; then
  git clone --depth=1 ...
else
  ...
  git fetch --depth=1 origin tag ${pkgver}-release
```

The `else` branch has never once executed here.

## Why this is possible at all

makepkg only removes `$srcdir` when `--cleanbuild` is passed:

```bash
if (( CLEANBUILD )); then
    rm -rf "$srcdir"; mkdir -p "$srcdir"
fi
cd_safe "$srcdir"
extract_sources
```

Neither `makechrootpkg` nor AURCache passes it -- our makepkg arguments are
`--noconfirm --noprogressbar --nocolor`, and `makechrootpkg`'s own `-C` is
*checkpkg*, an unrelated flag. So an existing tree is kept and sources are
re-extracted over it. Persistence requires no makepkg change; it requires only
that `/build` outlive the chroot.

## Layout

makepkg derives the tree from `BUILDDIR` (makepkg lines 1295-1300):

```bash
if [[ $BUILDDIR -ef "$startdir" ]]; then
	srcdir="$BUILDDIR/src"
else
	srcdir="$BUILDDIR/$pkgbase/src"
	pkgdirbase="$BUILDDIR/$pkgbase/pkg"
fi
```

Under `makechrootpkg`, `BUILDDIR=/build` and `startdir=/startdir`, so the second
branch applies and the tree is `/build/<pkgbase>/src`.

**makepkg therefore namespaces by pkgbase already.** A per-package host
directory would nest the name twice for no gain. The host side needs to key by
*platform* only:

```
<cache>/builddir/<platform>   →  bound at /build
                                 └── <pkgbase>/src
                                 └── <pkgbase>/pkg
```

Platform, because a build tree holds compiled objects. `SRCDEST` can be
pkgbase-only since downloads are architecture-independent; an x86_64 and an
aarch64 tree sharing a directory would corrupt each other. Keying by platform
also removes the same-package race on a worker that builds a native and an
emulated architecture.

`pkg/` persists too. It is staging output, recreated per run, and harmless.

## On by default

On by default, and the reason it can be is that the risk assumed at first
does not exist.

This is what an AUR helper on a workstation already does: `paru` and `yay` keep
their build trees between builds, across a very large number of users, and
trouble is rare. The chroot is still built from a fresh snapshot every time --
only the tree survives -- so the guarantee that actually matters, a clean
toolchain, is untouched.

Re-applied patches were the worry, and they are **not** a problem. Tested with
`libpng12`, whose `prepare()` does `patch -Np1` against a tarball source: three
consecutive `makepkg -o` runs against one persistent `BUILDDIR` all exited 0
with no "reversed (or previously applied)" anywhere. `extract_sources` unpacks
over the existing tree and bsdtar overwrites, so `prepare()` always sees
pristine sources. A file the tarball contains is restored even if the previous
run scribbled on it.

What survives re-extraction is everything the tarball does *not* contain --
confirmed in the same test: a planted `config.cache` and `png.o` were both
still there afterwards. That is the actual risk, and it is inseparable from the
benefit: leftover object files are what make an incremental rebuild fast, and
also what makes it wrong when compiler flags changed underneath them, or when a
killed build left something half-written. Such a failure produces a package
that is quietly incorrect rather than one that fails loudly, which is the
expensive kind.

That residual risk is real but rare, and it is the same mechanism as the
benefit -- so it is a reason to keep an escape hatch, not a reason to default to
throwing four hours of work away. The setting stays so a package that does turn
out to mind can be excluded without deleting directories on a worker by hand.

Resolved through `ApplicationSettings` like every other package setting, with
the established precedence `Package -> Env -> Global -> Default`. The packages
that want it are the ones where a rebuild costs hours, and there are few.

The setting decides whether the bind happens at all. When it is off the build
gets the ordinary `/build` inside the ephemeral chroot copy and nothing survives
it; when on, the host directory is bound over `/build` and makepkg's own
`<pkgbase>/` namespacing keeps packages apart inside it.

## Reclaiming space

**Two limits, because each is useless alone.** Before a build that will use a
persistent tree, drop trees oldest-first until the cache is within
`WORKER_BUILDDIR_MAX_BYTES` (default 200 GiB) *and* the filesystem has
`WORKER_BUILDDIR_MIN_FREE` bytes spare (default 50 GiB).

This was first designed as a size budget, implemented as a free-space floor,
and is now both. The floor alone was wrong: on a NAS pool with terabytes spare
it never triggers, so trees accumulate indefinitely -- the cache would grow
into the terabytes before anything reclaimed it, and "the disk is not full yet"
is not a reason to keep every build tree ever made. The cap is what bounds the
cache independently of how large the underlying storage is. But a cap alone
cannot see the rest of the machine, so the floor still covers a small disk, or
one shared with something else that grew.

Sizes come from a `.aurcache-size` stamp each build leaves in its tree, so
reclaim totals the cache by reading a handful of small files. A tree without a
stamp is walked once and stamped. This is what makes a cap affordable at all:
summing by walking would mean traversing 130 GB and millions of files on every
build, whereas measuring once after a build that already took hours costs
nothing noticeable.

- Whole trees, never partial contents: half a tree is worse than none, because
  makepkg would treat it as resumable.
- Before the build rather than after, so the limits bound usage going in.
- Never the tree the current build is about to use -- even when that tree is
  itself what breaches the cap, since evicting it defeats the point of asking.
- **Nothing is evicted while the cache is within its limits.** Disk nothing
  else needs is not worth reclaiming, and a tree kept is a rebuild avoided.
  Eviction happens only under real pressure, never as a tidy-up.
- **Abandoned trees go first**, meaning nothing has touched them in
  `WORKER_BUILDDIR_MAX_AGE_SECS` (default 30 days). This is what stops a tree
  outliving its package: a worker is never told that a package was deleted from
  the server, and an expensive tree is the last thing the next rule would give
  up, so `unreal-engine`'s 130 GB would otherwise sit there indefinitely.
  Ordering rather than a sweep -- while there is room, an abandoned tree costs
  nothing.
- **Then worst value density -- rebuild seconds per byte.** Not age, and not
  cost alone. LRU is backwards among live trees: the one worth keeping took
  four hours, and that is exactly the package built rarely enough to look stale
  beside a dozen small ones rebuilt daily. But cost alone is wrong the other
  way, because a huge tree only earns its place while there is room for it.
  `unreal-engine` at four hours over 130 GB is ~1.0e-7 s/byte; a thirty-second
  package over 200 MB is ~1.4e-7. The big tree is the *worst* value per byte
  despite costing the most, and freeing 130 GB by dropping it costs four hours
  where freeing the same space in small trees costs over five. So it is kept
  while there is room and given up first when space is genuinely short.
- A staleness threshold and one ratio, rather than a weighted score over age,
  size and cost: those weights would be invented, and there is no evidence here
  to choose them with. A tree stamped before cost was recorded sorts as free to
  discard.
- Worker settings rather than server ones: it is the worker's disk.
- Best-effort. Failing to reclaim is reported and the build proceeds; refusing
  to build over it would turn a full disk into an idle worker.
- The existing startup sweep is unaffected -- it removes `job-*` chroot copies,
  which are a different thing in a different directory.

Neither limit bounds a *single* build: `unreal-engine` can grow by ~130 GB after
the check passes. Nothing short of a filesystem quota would.

## What this does not solve

A retry still re-runs `prepare()` and `build()`. `prepare()` does
`git reset --hard`, which resets tracked files but leaves untracked build output
(`Intermediate/`, `Binaries/`), so UnrealBuildTool should skip most compilation
-- but that is an expectation, not a measurement, and it should be measured
before being claimed. The guaranteed saving is the clone; the incremental-build
saving is likely and unproven.

Exposing `--repackage` as a per-build action would make the salvage explicit
rather than depending on the build system's incrementality. That is a separate
change and is not proposed here.

## Rejected: making `BUILDDIR` equal `startdir`

makepkg takes a different branch when `BUILDDIR` *is* `startdir`, giving
`srcdir=$BUILDDIR/src` -- the layout a plain `makepkg` produces, where `..`
from `$srcdir` is the directory holding the PKGBUILD and its local sources.
Setting `BUILDDIR=/startdir` in the drop-in would therefore make PKGBUILDs like
`unreal-engine`'s `../unreal-engine.sh` resolve, and it would work: `/startdir`
is bind-mounted read-write (`--bind`, not `--bind-ro` -- the "not writeable"
warning is file ownership), makepkg already runs with `cd /startdir`, and
`makepkg.conf.d` drop-ins are sourced after `/etc/makepkg.conf`, so they
override the `BUILDDIR=/build` that `makechrootpkg` appends.

It is rejected because it makes AURCache lie. A package that built here would
still fail under `extra-x86_64-build`, or for anyone running `makepkg` in a
clean chroot -- so the service would ship packages whose PKGBUILDs are broken
and hide the evidence, moving the failure onto users' machines. `$startdir` is
discouraged in PKGBUILDs for exactly this reason; makepkg copies local sources
into `$srcdir`, and `$srcdir/unreal-engine.sh` is the portable form.

The right fix for such a package is to patch the PKGBUILD, which also benefits
everyone else building it.

Symlinking `/startdir`'s contents into `/build/<pkgbase>/` was considered and is
strictly worse: it produces a hybrid layout matching neither convention, and
risks shadowing the `src` and `pkg` directories makepkg creates there.

## Rejected: preserving the chroot copy

The first instinct after the `unreal-engine` failure was to stop passing
`makechrootpkg -T` on failure, keeping the chroot for post-mortem.

It is the wrong lever. What had value was `$srcdir`; the chroot was merely the
container it sat in. The chroot's own contents -- `base-devel`, `git`,
`openssh` -- are reproducible and cheap, a btrfs snapshot of the base costing
close to nothing. With a persistent `BUILDDIR` the chroot becomes genuinely
disposable, `-T` stays correct, and the thing worth keeping is no longer inside
the thing being deleted.

Keeping a failed chroot still has some value -- the exact set and versions of
installed `makedepends` explain some failures -- but it is a diagnostic
convenience, not recovery, and it would not have saved the four hours in any
automatic way.
