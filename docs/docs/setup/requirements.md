---
sidebar_position: 1
---

# Requirements

## Server

* Docker or Podman
* Linux 6.12 or newer, with Landlock enabled. Parsing a PKGBUILD executes it,
  so the server confines every parse and refuses to parse rather than run one
  unconfined — an older kernel means no package can be added
* No special performance requirements — it serves the UI, the API and the
  package repository, and does not build anything
* Disk for the package repository and the database

## Build worker

* Docker or Podman
* Permission to run the container `privileged` (or the equivalent capabilities:
  `SYS_ADMIN`, seccomp/apparmor unconfined) — each package is built in a
  `systemd-nspawn` chroot
* A writable tmpfs at `/run`
* Linux 6.7 or newer. Everything the worker stores lives in a btrfs
  [storage pool](../workers/storage-pool.md) with simple quotas, so a build
  cannot fill the host's disk
* Disk for that pool: `WORKER_DISK_MAX`, 200 GB by default. By default the pool
  is a sparse image file that takes space only as builds write. A zvol or a
  spare partition can back it instead, which is better on ZFS
* CPU and memory scale with the packages being built

Workers can run on the same host as the server or on separate machines,
including other architectures.
