---
sidebar_position: 5
---

# Docker Swarm setup

In Swarm, the **server** is an ordinary service and needs nothing special. The
**worker** builds each package in a `systemd-nspawn` chroot, so it needs the
permissions a chroot build requires:

```yaml
version: '3.8'
services:
  aurcache:
    image: ghcr.io/gyscos/aurcache-server:latest
    ...

  builder:
    image: ghcr.io/gyscos/aurcache-worker:latest
    privileged: true
    security_opt:
      - seccomp=unconfined
      - apparmor=unconfined
    cap_add:
      - SYS_ADMIN
      - SYS_PTRACE
    ...
```

Remember to add proper db, volumes and env. This is just to show the additional
permissions required.

Because the two are separate services, workers can be constrained to particular
nodes with the usual Swarm `placement` rules — which pairs well with
[routing](../workers/routing.md) when a package must be built on a specific
machine.

:::note Upgrading from a single service
The old `host build mode` / `DinD build mode` distinction described a single
container that spawned build containers. It no longer applies: builds now run on
worker services. If you are still running one container, see
[backward compatibility](./docker.md#backward-compatibility-the-hybrid-image).
:::
