---
sidebar_position: 1
---

# Requirements

## Server

* Docker or Podman
* No special performance requirements — it serves the UI, the API and the
  package repository, and does not build anything
* Disk for the package repository and the database

## Build worker

* Docker or Podman
* Permission to run the container `privileged` (or the equivalent capabilities:
  `SYS_ADMIN`, seccomp/apparmor unconfined) — each package is built in a
  `systemd-nspawn` chroot
* A writable tmpfs at `/run`
* Disk for the base chroot and the source/package caches (20 GB by default,
  tunable — see [worker configuration](../workers/configuration.md))
* CPU and memory scale with the packages being built

Workers can run on the same host as the server or on separate machines,
including other architectures.
