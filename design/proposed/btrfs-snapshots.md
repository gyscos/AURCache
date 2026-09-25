# More snapshots in the worker's pool

Every chroot worker now keeps everything it stores in a btrfs storage pool
(`design/implemented/build-disk-quota.md`), so btrfs snapshots are available on
every worker, not only on hosts that happen to be btrfs. A snapshot is instant,
and under simple quotas a new snapshot is charged only for what is written into
it afterwards. That makes it cheap to get three things: a rollback point,
isolation between writers, and a copy to inspect later. This doc collects the
places where the worker could use them.

Status: **Proposed** · Last updated: 2026-09-25

---

## Where snapshots are used today

One place: each build's chroot is a snapshot of the base (`<pool>/job-<id>` from
`<pool>/root`), made by the worker before `makechrootpkg` runs and deleted when
the build ends. Everything else in the pool is written in place:

- The base chroot is upgraded in place by `arch-nspawn root pacman -Syu`, under
  devtools' `root.lock`.
- Each package's source cache (`cache/srcdest/<pkgbase>`) and kept build tree
  (`cache/builddir/<arch>/<pkgbase>`) is a subvolume, written directly by the
  build that uses it. Builds of the same package are serialized on the source
  cache (`SrcdestLocks`).

## Accounting, which every idea below has to respect

Simple quotas charge an extent to the subvolume that wrote it, for as long as
the extent exists, and never move that charge:

- **A snapshot charges nothing on its own.** What it shares with its origin
  stays charged to whichever subvolume wrote it.
- **A deleted subvolume's group lingers while its extents do.** When something
  else still references its extents (a snapshot, a reflink), its level-0 group
  stays as a `<squota space holder>` carrying that charge, and it keeps its
  relations to the groups above it. So the charge stays under the pool's total,
  as long as we never destroy a group that still carries charge. The pool code
  only destroys groups btrfs agrees are empty (`tidy_groups`), which keeps this
  true.
- **Never reflink out of a build into long-lived storage.** A reflinked extent
  stays charged to the build that wrote it, so it is invisible to the
  destination's figures. This is why a job's pacman cache already lives inside
  the `pacman-pkg` subvolume rather than the build's. The same rule applies
  below: data moving from a build into a cache is copied, never reflinked.

## 1. Refresh the base as a new snapshot and swap it in

**Today.** A refresh upgrades `root` in place. It takes `root.lock` exclusively;
a build start that finds it held only shared (for the instant of its own
snapshot) defers the refresh. An upgrade that breaks halfway (a failing hook, a
half-installed package, a full disk) leaves the base broken for every build
after it, and nothing puts it back.

**Proposal.**
1. `btrfs subvolume snapshot root root.next`.
2. Upgrade `root.next`, with the same `arch-nspawn … pacman -Syu` and
   `ensure_multilib`.
3. Check it: at least a clean exit and `pacman -Dk`. A failed or failed-check
   upgrade deletes `root.next` and leaves `root` as it was.
4. Swap: `renameat2(root.next, root, RENAME_EXCHANGE)`, so the path `root`
   atomically becomes the new base and `root.next` the old one. Then rename the
   old one to `root.prev` and delete the `root.prev` before it.

A build snapshots whatever `root` is at the instant it asks, before or after
the swap, and is never exposed to a base halfway through an upgrade. The
refresh never waits on builds and builds never wait on it, so `root.lock` and
the deferral logic (`try_lock_base`, `BaseLock::Busy`) go away. `root.prev` is
a rollback: if a new base turns out bad, renaming it back restores the last
one, which could be an explicit operation (`aurcache-cli worker …`) later.

**Accounting.** `root.next` is made at the pool's top, which the total does not
cover, so it is assigned to `2/0` on creation, as `mkarchroot`'s base is today
(`charge_to_total`). What the upgrade writes is charged to `root.next`. When
the old base is deleted while builds still hold snapshots of it, its group
becomes a space holder for what they share, still under `2/0`, and it empties
as those builds end.

**Cost.** One snapshot per refresh (milliseconds), plus the old base's
exclusive extents held until `root.prev` is replaced: roughly one refresh's
worth of upgraded packages.

**Rejected before, and why that no longer holds.** `overlay-chroot.md`
rejected "a new base per refresh" because on the filesystems it targeted a
copy was a full write of the chroot (~62 GB a day at a 30-minute interval).
Inside the pool a copy is a snapshot, so the objection is gone.

## 2. Per-build snapshots of the source cache, merged back with git

**Today.** A build writes its package's source cache directly: downloads, and
`git fetch` into bare mirrors. So two builds of one package cannot run at
once, and a source cache *shared across packages* -- the gcc and glibc
toolchain case in the deferred shared-SRCDEST idea -- cannot exist, because two
builds would write the same mirror concurrently. A fetch that corrupts a mirror
is caught only afterwards, by `wipe_srcdest`'s self-heal.

**Proposal.**
1. At lease time, snapshot the source cache into the build:
   `cache/srcdest/<key>` → `<pool>/job-<id>.src`, put in the build's group
   (`1/<1000+id>`) and bound at `/srcdest`. The build fetches into its own copy.
   Nothing it does reaches the shared cache while it runs.
2. When the build succeeds, merge its copy back **at the git level, not by
   replacing the subvolume**, under a short per-key lock:
   - **git mirrors:** in the shared mirror, fetch from the build's copy. That
     adds every object the build has. Git objects are only ever added, so a
     union of two builds' fetches is always safe.
   - **plain downloads** (tarballs): copy the files the shared cache lacks,
     with `--reflink=never` (see Accounting).
3. A failed build's copy is discarded, fetches and all: a corrupt mirror never
   reaches the shared cache.

**Why not promote the whole snapshot.** Two builds that snapshot the same
mirror and fetch at different times each end with a complete, consistent
mirror. Promoting either one discards what only the other fetched. A kept build
tree's checkouts are `git clone -s` of the mirror, so they own no objects and
borrow them through `alternates`: a checkout made by the losing build can
reference objects the promoted mirror lacks, and that package's next build
fails with `bad object … did not send all necessary objects`. That is the
failure `wipe_borrowed_checkouts` exists for. A git-level merge keeps the
union, so every checkout's objects stay present.

**Refs.** Objects merge freely; refs need a rule.
- A ref only moves forward when the build's value is a descendant of the
  shared one (a fast-forward).
- A ref that is not (upstream force-pushed) takes the value from whichever
  build *fetched* later -- each copy records when it fetched -- not whichever
  finished later.
- The older value's objects stay in the mirror either way, so a checkout still
  pointing at them keeps working. They are pinned under
  `refs/aurcache/kept/…` so a later `git gc` does not prune what a checkout
  borrows, and those pins are dropped when the package's kept tree is evicted.

Open detail: whether to fetch with an explicit refspec and apply the rule in
our code, or fetch into a staging namespace (`refs/aurcache/incoming/<id>/*`)
and move refs from there. The second keeps every decision out of git's
`+`/no-`+` semantics.

**What it enables.** Builds of one package no longer serialize on its sources,
and the shared cross-package source cache becomes possible: the cache key
becomes a *source* (a mirror URL) rather than a pkgbase. Eviction then works
on shared keys, so wiping one no longer means "this package's sources are
bad".

**Accounting.** What a build fetches is charged to its own copy, inside its
per-build limit: its downloads are part of its build, as they are not today. The
merge writes new packs into the shared mirror, charged to the shared cache.
`git fetch` from a local path goes through its transport and writes objects
normally; unlike `git clone --local`, it does not hardlink. Downloads are copied
without reflink for the same reason. When the build's copy is deleted, its
extents are freed.

**Cost.** A snapshot per build (milliseconds) and a local fetch per success,
proportional to what the build fetched, not to the mirror's size. The lock is
held only for that local merge.

## 3. A rollback point for kept build trees

**Today.** A build writes its kept tree in place. One that dies partway leaves
a half-updated tree, which makepkg treats as resumable next time, and a stale
checkout in it can fail every retry identically (the self-heal in
`wipe_borrowed_checkouts` and the stale-checkout detection in `job.rs`).

**Proposal.** Snapshot the tree before the build (`<tree>` → `<tree>.pre`). On
success, delete the snapshot. On failure, either:
- **roll back:** delete the tree and rename the snapshot into its place. The
  next build resumes from the last good state; or
- **discard:** delete both. The next build starts cold, which is always safe.

**Accounting caveat, which decides between them.** The kept tree's size is read
from its own subvolume's group. After a rollback, the tree is the snapshot,
whose own group carries only what was written into it since (nothing), while
its real contents stay charged to the deleted original, now a space holder. The
total stays right, but that tree then reads as nearly empty to eviction, which
would never pick it by size until it is rebuilt. So either:
- rollback also re-measures the tree the old way (the size stamp) and eviction
  prefers the stamp for such a tree, or
- failures discard instead, which is simpler and always accurate, at the price
  of a cold rebuild.

Discard is the safer default. Rollback is worth it for packages whose tree
takes hours to rebuild (unreal-engine), which could make it a per-package
choice.

**Cost while the build runs.** The snapshot holds the tree's old extents that
the build overwrites, charged to the tree, until the build ends. For a build
that rewrites most of its tree, that is up to one extra tree's worth of disk.

## 4. Keep a failed build's chroot for a while

**Proposal.** When a build fails (not when it is canceled or times out), keep
its snapshot and workdir for a configurable time (`WORKER_KEEP_FAILED`, off by
default) instead of deleting them at once. An operator can then enter the exact
state it failed in: `systemd-nspawn -D <pool>/job-<id>`, with the workdir at
`<pool>/job-<id>.data`. The release happens later, from the sweep.

- **The sweep** keeps what is kept and still within its time, and deletes the
  rest. The startup sweep does the same, instead of treating every leftover as
  garbage.
- **Its quota group** keeps its limit, so a kept build can never grow, and it
  stays under the total: kept builds count against `WORKER_DISK_MAX`, and when
  the pool is short of room they are the first things deleted, before caches.
- **Where it is shown.** The build's report could say it was kept and where; a
  "debug on worker" action is further off.

**Cost.** Up to one build's disk per kept failure, for the time set.

## Rejected: a per-package chroot with its dependencies installed

Keeping, per package, a snapshot of the chroot after its dependencies were
installed, and snapshotting from that next time, would skip the dependency
install step. But dependencies removed from the PKGBUILD would stay installed,
and `overlay-chroot.md` already rejected "a chroot per package" for exactly
that: a leftover `-dev` package changes what `configure` detects, and a
package can link against something it never declared.

## Order

1. **Base refresh by snapshot and swap.** It removes a failure mode (a broken
   base) and a source of contention (the refresh lock), and it is small: the
   refresh path and the removal of the lock logic.
2. **Per-build source snapshots with git-level merge.** The largest, and the
   one that unlocks something new (the shared cross-package source cache).
   Start with same-package concurrency, then change the cache key.
3. **Keep failed builds.** Small and independent, useful for debugging.
4. **Kept-tree snapshots.** Discard on failure first; rollback per package only
   if a long rebuild justifies it.
