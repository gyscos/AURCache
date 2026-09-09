---
sidebar_position: 1
---

# Worker Configuration

:::note Upgrading from a single container
If you are still running AURCache as one container, it keeps working through the
[hybrid compatibility image](../setup/docker.md#backward-compatibility-the-hybrid-image),
which is deprecated. Everything on this page applies to a worker you run
yourself.
:::


A build worker is a long-lived process that enrolls with AURCache, claims build
jobs, builds each package in its own `devtools` chroot, and uploads the result.
Workers are configured entirely through environment variables — AURCache's
Settings pages do not override them, and a worker reports its configuration
every time it starts, so changing a value means restarting that worker.

## Identity and capabilities

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_NAME` | String | Name shown on the Workers page | hostname |
| `WORKER_ARCHES` | String | Architectures this worker builds natively, comma or space separated | host architecture |
| `WORKER_EMULATED_ARCHES` | String | Architectures it can build via emulation (qemu-user + binfmt) | empty |
| `WORKER_CONCURRENCY` | Integer | Maximum builds at once | 1 |

:::note Raising concurrency
Each build is already parallel — `MAKEFLAGS=-j$(nproc)` is set inside every
build — so `WORKER_CONCURRENCY=N` runs up to N × CPU-count compiler processes,
and each concurrent build additionally holds its own chroot copy and package
cache. Raise it when you have memory and disk headroom, not simply because the
machine has cores; two or three is a large increase for most hosts.
:::

A foreign architecture is only ever emulated when **no** worker builds it
natively. That keeps an aarch64 job waiting for the aarch64 machine instead of
crawling through emulation somewhere else.

## Routing

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_PACKAGES` | String | Exact pkgbase names this worker is provisioned for | empty |
| `WORKER_PRIORITY` | Integer | Scheduling preference; higher wins | 0 |

See [Routing builds to specific workers](./routing.md).

## Mirrorlist

By default a worker needs none of this: the server sends the mirrorlist for the
build's architecture with each job, and a worker that gets none uses its image's
own `/etc/pacman.d/mirrorlist`.

Set one of these when this worker's mirrors beat the server's — a worker on
other hardware, or across a slow link from the mirror the server prefers.

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_MIRRORLIST_SERVERS` | String | Semicolon-separated mirror URLs, used instead of the server's | empty |
| `WORKER_MIRRORLIST_FILE` | Path | A ready mirrorlist file inside the container | empty |

`WORKER_MIRRORLIST_SERVERS` wins if both are set. Either one makes the worker
tell the server not to send a mirrorlist at all, so nothing is transferred that
would only be discarded.

See [Mirrorlist](../Configuration/mirrorlist.md) for the server side.

## Build credentials

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_GIT_SSH_KEY` | Path | SSH key for authenticated sources. When set, no key is generated | generate one |
| `WORKER_SSH_KNOWN_HOSTS` | Path | `known_hosts` to trust when fetching sources | accept on first use |
| `WORKER_BIND_MOUNTS` | String | Extra `host:chroot` mounts, comma separated | empty |

See [Build credentials](./credentials.md).

## Caches

Workers keep two caches: downloaded upstream sources (`SRCDEST` — tarballs and
VCS checkouts) and downloaded pacman packages. They are budgeted separately
because they differ enormously in size and in what it costs to refill them: a
few large VCS checkouts sharing one budget would evict every cached package,
leaving each build to re-download its dependencies.

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_CACHE_MAX_SIZE` | Size | **Total** cache budget, split evenly between the two pools | `20G` |
| `WORKER_SRCCACHE_MAX_SIZE` | Size | Source budget; overrides half the total | half of total |
| `WORKER_PKGCACHE_MAX_SIZE` | Size | Package budget; overrides half the total | half of total |
| `WORKER_CACHE_TTL` | Integer | Evict sources unused for this many seconds (`0` disables) | 30 days |
| `WORKER_PKGCACHE_TTL` | Integer | Evict packages older than this (`0` disables) | `0` |

Sizes accept `20G`, `500M` or a plain byte count. Setting a budget to `0`
disables size-based eviction for that pool.

Setting both per-pool budgets makes the split ratio configurable without a
separate knob for it. A pinned pool always wins: the total is a default for
whatever is left unset, not a cap enforced over values you wrote down.

**Package TTL defaults to off** deliberately. A cached package's timestamp is
when it was *downloaded*, not when it was last used — pacman does not touch it
on a cache hit — so age-evicting would discard a package used in every build
simply for being old. Size pressure is the honest bound for that pool.

## Locations

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_DATA_DIR` | Path | Worker identity, credentials, chroots | `/var/lib/aurcache-worker` |
| `WORKER_CHROOT_DIR` | Path | Base chroot and per-job copies | `<data dir>/chroot` |
| `WORKER_CACHE_DIR` | Path | Source and package caches | `/var/cache/aurcache-worker` |
| `WORKER_CHROOT_REFRESH_INTERVAL` | Integer | Seconds a `pacman -Syu`'d base chroot counts as current (`0` refreshes before every build) | `900` |
| `WORKER_CHROOT_OVERLAY` | `auto`/on/off | Mount each build's chroot as an overlay instead of copying the base | `auto` |

The base chroot is brought up to date with `pacman -Syu` before a build, but
not more often than `WORKER_CHROOT_REFRESH_INTERVAL`. The refresh takes around
13 seconds and only one build may hold the chroot while it runs, so paying it
per build delayed every build start in a burst; measured over a day on one
worker, 23 of 26 refreshes upgraded nothing at all, because Arch's repositories
move a few times a day rather than a few times an hour. Lower it if you would
rather have the newest dependencies than the fastest starts; `0` restores the
old behaviour.

`WORKER_DATA_DIR` **must be a persisted volume.** It holds the worker's identity
and its generated SSH key; if it is lost, the worker re-enrolls as a new,
unapproved worker and generates a new key, which the remote will no longer
accept.

### Overlay chroots

`makechrootpkg` gives each build a copy of the base chroot. On btrfs that copy
is a snapshot and costs nothing; everywhere else it is an `rsync` of the whole
chroot -- around 800 MB per build, which on an SSD is both slow and wear you
did not ask for.

`WORKER_CHROOT_OVERLAY` mounts the base read-only as an overlay's lower layer
instead, with a directory of the build's own on top. The mount takes about 15
milliseconds and the upper layer holds only what the build changed, which for
an ordinary package is a few hundred kilobytes. The base cannot be written
through the mount, so builds are as isolated from each other as they were with
copies.

It defaults to `auto`, which decides at startup, because the right answer is a
property of the machine rather than a preference:

* On **btrfs** the worker copies. A copy there is already a snapshot -- 0.29
  seconds and no space -- so an overlay would save nothing and cost something
  (see below).
* Anywhere else the worker **tries to mount an overlay**, and uses one if the
  kernel allows it. The check is a real mount, because nothing else is
  conclusive: an upper layer needs whiteouts and trusted extended attributes,
  which ext4, XFS and btrfs have, ZFS has from 2.2, and NFS does not -- and a
  container may not be permitted to mount at all.
* If that mount fails, it copies, and says so once at startup.

Set it to `1` or `0` to decide yourself. An unrecognised value means `auto`, so
a typo cannot quietly disable something you were trying to enable.

Refreshes accumulate as layers, and layers are merged back into the base once
there are enough of them. Merging runs alongside builds: it reads what they
read, writes the new base somewhere nothing is reading, and publishes it with a
rename, which a mounted chroot does not notice. The base and layers it replaces
are retained -- almost free, since the new base is hardlinked from the old --
and deleted once no build is reading them.

A live overlay's lower layer must not change, which is why a refresh publishes
a layer rather than rewriting the base: nothing a running build reads is ever
modified, so refreshes keep to their interval however busy the worker is.

### Putting the chroot on other storage

Builds are the bulk of a worker's disk use, and some packages are extravagant:
`unreal-engine` needs around 300&nbsp;GB live. `WORKER_CHROOT_DIR` moves the base
chroot and the per-build copies somewhere with room, leaving the small stuff
(`WORKER_CACHE_DIR` is usually a few GB) where it is.

Whatever you point it at has to behave like a real Unix filesystem. A base
chroot contains setuid binaries, thousands of hardlinks, files owned by several
users, and a couple of files carrying `security.capability` extended attributes
(`newuidmap` and `newgidmap`, used for user-namespace id mapping).

- **A local filesystem, or a network block device** (iSCSI, formatted on the
  worker) supports all of it, because the filesystem is local either way.
- **NFS** works if the export is `no_root_squash` and the client does not mount
  it `nosuid`. Two caveats: `security.*` xattrs are not carried over NFS, so
  those file capabilities are lost -- harmless unless a package builds something
  in a rootless user namespace -- and builds are a many-small-files workload,
  which NFS is not fast at.
- **SMB/CIFS** cannot represent Unix ownership, setuid bits or hardlinks. The
  chroot cannot be created on it at all.

:::tip btrfs makes per-build copies free
`makechrootpkg` snapshots the base chroot when the chroot directory is btrfs and
`root` is a subvolume, and copies it wholesale otherwise -- 7--12&nbsp;GB of
rsync per build on ext4, against a copy-on-write snapshot that costs almost
nothing. If you are formatting new storage for this, make it btrfs and create
`root` as a subvolume. This applies to local and block storage; a chroot on NFS
is a directory tree on the server's filesystem, so it takes the copying path
whatever the server's pool is made of.
:::

## Timing

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_BUILD_TIMEOUT` | Integer | Kill a build after this many seconds (`0` disables) | 3 hours |
| `WORKER_POLL_INTERVAL` | Integer | Seconds between job claims when idle | 10 |
| `WORKER_HEARTBEAT_INTERVAL` | Integer | Seconds between heartbeats | 15 |
| `LEASE_TTL` | Integer | Seconds before a silent worker's build is requeued | 60 |
| `WORKER_KEYSERVER` | String | Keyserver for `validpgpkeys` | `hkps://keyserver.ubuntu.com` |

Very large packages need `WORKER_BUILD_TIMEOUT` raised — the three-hour default
is generous for ordinary packages and nowhere near enough for something like a
browser or a game engine.

## Connecting to AURCache

| Variable | Type | Description | Default |
|---|---|---|---|
| `AURCACHE_URL` | String | Worker protocol endpoint (HTTPS + mTLS) | `https://localhost:8080` |
| `AURCACHE_SERVER_CA_FINGERPRINT` | String | Pin the server's CA (SHA-256 hex) | trust on first use |
| `AURCACHE_ENROLLMENT_DIR` | Path | Shared volume for zero-touch approval | `/enroll` |
| `AURCACHE_ENROLLMENT_TOKEN` | String | Shared secret for non-interactive approval | none |

## Server-side settings

These are set on the AURCache server, not the worker.

| Variable | Type | Description | Default |
|---|---|---|---|
| `AURCACHE_WORKER_PORT` | Integer | Port for the worker protocol listener | 8083 |
| `AURCACHE_WORKER_IMAGE` | String | Worker image named in the copy-and-run command the **Workers** page shows when no worker exists yet | `ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest` |
| `WORKER_SPILL_DELAY` | Integer | Seconds before priority stops holding a job back | 60 |
| `WORKER_LIVENESS_TIMEOUT` | Integer | Seconds before a quiet worker stops counting as available | 60 |
| `MAX_ATTEMPTS` | Integer | Requeues before a build is failed for good | 3 |
| `WORKER_CERT_VALIDITY_DAYS` | Integer | Lifetime of an issued worker certificate | 3650 |

Set `AURCACHE_WORKER_IMAGE` when you publish the worker image to your own
registry or want the suggested command to pin a tag — it only changes what that
one command displays, nothing about how workers connect.

Worker certificates are long-lived on purpose. A certificate grants nothing on
its own — authorization is the worker's approval status, which an operator can
revoke at any moment — so a short lifetime would add no security and would
eventually break a worker with no renewal path.
