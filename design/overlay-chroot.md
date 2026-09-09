# Overlay chroots and update layers

Plan for making a build's chroot cost nothing to create on filesystems that
cannot snapshot, and for refreshing the base without waiting for the builds
using it.

Status: **partly implemented.** The overlay strategy and its startup detection
are in (`aurcache-worker/src/chroots.rs`). Update layers -- the part that makes
refreshing free of the builds in flight -- are designed here and not built.

---

## The cost that prompted it

`makechrootpkg` gives every build a copy of the base chroot. On btrfs that copy
is a snapshot and the whole question is uninteresting:

| | make a copy | delete it |
|---|---|---|
| btrfs snapshot | 0.29 s, no space | 0.015 s |
| `cp -a --reflink` | 2.18 s, no space | 1.6 s |
| `rsync -a --delete` (what devtools does elsewhere) | **810 MB copied** | seconds |

`persistent-build-directory.md` says a chroot is "reproducible and cheap, a
btrfs snapshot of the base costing close to nothing", and on the machine it was
written for that is true. It is not true anywhere else: the ZFS worker in this
deployment rsyncs 810 MB per build, which at 26 builds a day is ~21 GB of
writes for chroots that are byte-identical to the one before.

An overlay mount replaces the copy outright:

| | cost |
|---|---|
| `mount -t overlay` | **0.015 s** |
| upper layer after a real package build | **132 KB** |

The isolation is stronger than a copy's, not weaker. A copy is merely *not*
written to; a lower layer cannot be written through the mount at all, and the
kernel enforces that rather than us hoping every writer replaces files instead
of modifying them.

## Why the base cannot simply be refreshed

The worker brings the base chroot up to date with `arch-nspawn root pacman
-Syu`. A copy is a point-in-time tree, so refreshing afterwards cannot reach
it. **A live overlay's lower layer is not** -- overlayfs documents changes to a
mounted lower as undefined, and it caches dentries and inodes, so a build can
see a half-updated file, a stale listing or `ESTALE` depending on what it had
already looked at.

So an overlay lease holds `root.lock` shared for the whole build, and the
refresh takes it exclusively. That is correct and it is why the first attempt
at it stalled the worker: the refresh blocked on the lock while holding the
mutex every job start passes through, so one four-hour build meant no build
started at all. It now tries without waiting and defers instead, which turns a
stall into staleness -- a worker that is never idle never finds the base free.

Both problems have the same root: **the base is mutable and shared**.

## Layout

Make it immutable instead, and express a refresh as something new rather than a
change to something old.

```
<chroot_dir>/root                     base, created once by mkarchroot, then frozen
<chroot_dir>/.overlay/updates/0001    one refresh: only the files it changed
<chroot_dir>/.overlay/updates/0002
<chroot_dir>/.overlay/job-N/upper     one build's writes
<chroot_dir>/.overlay/job-N/work
<chroot_dir>/job-N                    what makechrootpkg is pointed at
```

A build mounts the stack that exists when it starts:

```
lowerdir=updates/0002:updates/0001:root,upperdir=.overlay/job-N/upper,workdir=…
```

overlayfs takes lower layers left-to-right, newest first, so a file in `0002`
shadows the same file in `root`.

## Refreshing becomes a new layer

To refresh, mount the current stack with a *new* layer as the upper, run
`pacman -Syu` inside it, and unmount:

```
lower = updates/0001:root      upper = updates/0002.tmp
arch-nspawn <merged> pacman -Syu --noconfirm
rename updates/0002.tmp -> updates/0002        (publication is the rename)
```

What lands in `0002` is exactly the files pacman changed. Nothing that any
build is reading is touched, so:

* a refresh never waits for a build, and a build never waits for a refresh;
* `root.lock` stops being load-bearing for the overlay path -- it is still what
  the copy path uses, where devtools copies from a base we do update in place;
* the write volume of a refresh is the size of the upgrade, not the size of the
  chroot.

That last point is the one that decides between this and the simpler
alternative of a whole new base per refresh; see below.

Publication is a rename so a crash mid-refresh leaves `.tmp` rubbish rather
than a half-written layer in the stack. Builds already running keep the stack
they mounted; the next build gets the longer one.

## Flattening

The stack grows by one directory per refresh -- three a day, measured. Lookup
cost grows with it, and `lowerdir` lists are not unbounded (Docker caps its
equivalent at 128 layers).

Flattening replaces `root` with the merged view and drops the layers. It is a
full write, so it happens rarely -- once the stack passes a threshold, at the
first moment no overlay is mounted. That "no overlay is mounted" test is not a
nicety: the layers being merged are the lower layers of any live build, and
removing them under one is the same undefined behaviour the whole design exists
to avoid. If the worker is never idle, the stack keeps growing and the flatten
keeps deferring; that is a warning, not a failure.

## Crash recovery

The startup sweep already unmounts before removing, deepest first, because a
copy left mounted is a directory `rm` cannot empty. Layers need two more rules:

* an `updates/*.tmp` is a refresh that died; delete it.
* a layer is never deleted because it looks old. It is deleted by flattening,
  and only when nothing is mounted -- reference counting a directory that
  outlives the process means reading `/proc/self/mountinfo`, not remembering.

## Rejected: a whole new base per refresh

The obvious form of "make it immutable" is to copy `root` into `root.N+1`,
update that, and point new builds at it. It is simpler -- no stack, no
flattening, no `lowerdir` limit -- and on btrfs or ZFS it is nearly free.

It is rejected because of what it does on the filesystems this is *for*. A copy
there is a full write of the chroot, and refreshes are driven by a timer:

| | writes/day |
|---|---|
| copy per build (today) | ~21 GB |
| new base per refresh, every 30 min | **~62 GB** |
| new base only when the repositories changed | ~4 GB |
| update layers | tens of MB |

Refreshing on a timer would write more than the problem it was meant to fix.
Measured over a day here, 26 refreshes produced three transactions that changed
anything at all -- nine packages -- because Arch's repositories move a few times
a day, not a few times an hour.

## Rejected: hardlinked copies

`cp -al` is the cheapest copy of all -- 0.95 s for 46,537 files, against 2.18 s
for reflinks -- and works on every filesystem. It also does not isolate
anything: a hardlink is one inode with two names, so an in-place write inside
one chroot appears in the base and in every other chroot sharing it. Measured,
on the first transaction:

```
append to /etc/passwd in the copy   → template /etc/passwd:  1 hit
pacman -S licenses in the copy      → template pacman.log:   30067 → 30390 bytes
```

Most package installs replace files rather than modifying them, so a prototype
looks perfect and the corruption surfaces weeks later. Debian's `cowbuilder`
needs `cowdancer`, an `LD_PRELOAD` shim that breaks each link before a write,
for exactly this reason.

## Rejected: reflink copies as the general answer

`cp -a --reflink=always` is genuinely portable across btrfs, XFS and ZFS ≥ 2.2
-- it works today in the ZFS worker's container -- and it leaves an ordinary
directory, so deletion needs no subvolume handling at all. But it is 2.18 s
against 15 ms, it is a full metadata walk of 46,537 files, and ext4 cannot do it
at all. It remains the right fallback to consider for `Strategy::Copy` on
filesystems without snapshots, and it does nothing about refreshing a base that
builds are reading.

## Rejected: a chroot per package

Keeping one chroot per package and re-syncing it from the base is cheap in the
steady state -- an incremental `rsync -a --delete` from the template measured
0.47 s -- and `--delete` even restores cleanliness. It was rejected on
isolation: a dependency installed for an earlier build of that package stays
installed, and a leftover `-dev` package changes what `configure` detects, so a
build can link against a library it never declared and ship a package that is
broken for everyone who installs it. Overlays give the same saving with the
clean room intact.

## What this does not solve

Workers whose chroot is on NFS, or on ZFS below 2.2, cannot mount an upper
layer and keep copying. The detection handles that, and they keep the
in-place refresh and its exclusive lock, which is correct for them because a
copy holds the lock for seconds rather than for a build.

Flattening still needs an idle moment. A worker that never idles accumulates
layers until it does.

And `makechrootpkg` still copies *something* on the copy path: this changes
nothing for btrfs workers, deliberately, because a snapshot already costs
0.29 s.
