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

`WORKER_DATA_DIR` **must be a persisted volume.** It holds the worker's identity
and its generated SSH key; if it is lost, the worker re-enrolls as a new,
unapproved worker and generates a new key, which the remote will no longer
accept.

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
