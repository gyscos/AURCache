---
sidebar_position: 2
---

# Docker Compose setup

AURCache runs as two pieces:

- the **server** (`aurcache-server`) — package database, web UI, API, and the
  pacman repository;
- one or more **build workers** (`aurcache-worker`) — each builds packages in a
  clean `devtools` chroot and uploads the results.

They talk over mutual TLS on port 8083, so a worker can live in the same compose
stack, on another machine, or on another architecture.

## Docker tags

| Image | What it is |
|---|---|
| `aurcache-server` | The server. This is what most setups want. |
| `aurcache-worker` | A build worker. Multi-arch (`amd64`, `arm64`, `arm/v7`). |
| `aurcache` | The hybrid compatibility image — **deprecated**, see [below](#backward-compatibility-the-hybrid-image). |
| `aurcache-builder` | Spawned per build by the hybrid image's legacy builder. Not used by the split setup. |

Each is published as `:latest`, `:<version>` for a release tag, and the server
also as `:git` for the current master branch.

## Single host

The bundled [`docker-compose.yaml`](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/docker-compose.yaml)
starts a server plus one local worker and needs no edits:

```bash
curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/docker-compose.yaml
docker compose up -d
```

The worker auto-enrolls through the shared `enroll` volume — being able to write
to a volume the server reads is what proves it is trusted, so there is no token
to configure and no approval to click.

See the [Quick Start](../overview/quick-start.md) for the same setup with
PostgreSQL.

## Why the worker needs `privileged`

Each package is built in its own `systemd-nspawn` chroot, which needs mounts and
namespaces an ordinary container forbids. `privileged: true` plus a tmpfs `/run`
is the tightest configuration that works everywhere; if your host allows it you
can narrow this to specific capabilities with seccomp/apparmor unconfined.

Only the **worker** needs this. The server handles no build payloads and runs
unprivileged.

## More workers

Scale build throughput on the same host:

```bash
docker compose up -d --scale builder=3
```

Workers on separate hardware, or on a foreign architecture, are covered in
[Build Workers](../workers/configuration.md) and
[`docker-compose.remote-worker.yaml`](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/docker-compose.remote-worker.yaml).

## Backward compatibility: the hybrid image

:::warning Deprecated
The `aurcache` image runs the server *and* a build worker in one container. It
exists so that deployments predating build workers keep working after an
upgrade without editing their compose file. It will be removed in a future
release — migrate to `aurcache-server` + `aurcache-worker` when convenient.
:::

If you already run AURCache as a single container, pulling the new `aurcache`
image keeps it building with no changes. On startup it launches the server, then
an embedded worker that enrolls over loopback with a secret generated inside the
container.

Which builder it uses is decided from your existing configuration:

| Your setup | Embedded builder |
|---|---|
| `privileged: true` (the old DinD mode) | `devtools` chroot — the same builder the split setup uses |
| `BUILD_ARTIFACT_DIR` set (the old host mode) | Legacy container builder, spawning a container per package against the mounted Docker socket |
| Neither | None. The server starts and the UI reports that no worker is available. |

`BUILD_ARTIFACT_DIR` is not a hint here — in the old code it *was* the
definition of host build mode, so a deployment that sets it gets the behaviour
it had before.

The legacy container builder is the more limited of the two: it builds in a
reused image rather than a clean chroot, and supports neither the source and
package caches nor [build credentials](../workers/credentials.md). It is
provided for continuity, not as a supported build strategy.

### What you gain by migrating

- Builds move off the server host, or onto several machines.
- Workers for other architectures, native or emulated.
- Routing: reserve particular packages for particular workers, and prefer fast
  workers over slow ones — see [Routing](../workers/routing.md).
- Source and package caches, so rebuilds do not re-download the world.
- Build credentials for packages whose sources need authentication.

### Migrating

1. Split your single service into two, following the
   [Quick Start](../overview/quick-start.md) — server on the `aurcache-server`
   image, worker on `aurcache-worker`.
2. Keep your existing `/app/repo` and database volumes on the server. Nothing
   about the repository or package history changes.
3. Drop `BUILD_ARTIFACT_DIR`, the Docker socket mount, and the `artifact_cache`
   volume — the worker uploads its packages over the API instead of through a
   shared directory.
4. Move `privileged: true` from the server to the worker, and give the worker a
   tmpfs `/run`.

The worker your hybrid container was running keeps its history; a new worker
simply enrolls alongside it, and you can retire the old row from the Workers
page — see [Managing workers](../workers/managing.md).
