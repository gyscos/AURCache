---
sidebar_position: 4
---

# Managing workers

The **Workers** page lists every worker that has ever enrolled, with its status,
architectures, reserved packages, priority, build strategy and version.

## Build strategies

The **Type** column says how each worker builds:

| Type | Meaning |
|---|---|
| `chroot` | The `devtools` chroot worker — the supported strategy. |
| `docker` | The legacy container builder, **deprecated**. Shown highlighted. |
| `—` | The worker enrolled before workers reported this; restarting it fills it in. |

A worker reports whatever it calls itself, so a future build strategy appears
here without the server needing to know about it in advance.

This is the quickest way to answer "is anything still on the legacy builder?"
before retiring it — it builds in a reused container image rather than a clean
chroot, and supports neither build caches nor build credentials.

## Approving

A newly enrolled worker is `pending` and cannot build until approved. Approve it
from the Workers page, or let one of the non-interactive paths do it: a shared
enrollment volume (`AURCACHE_ENROLLMENT_DIR`, used by the bundled compose
setup), a pre-approved fingerprint list, or a shared token
(`AURCACHE_ENROLLMENT_TOKEN`).

Auto-approval only ever applies to a worker that has never been approved. It
will not re-approve one you revoked — an explicit decision outranks a
convenience setting.

## Retiring a worker

**Revoke it.** Revoking refuses the worker's certificate from that moment,
releases any packages it had reserved through `WORKER_PACKAGES`, and requeues
whatever it was building so another worker picks the job up.

There is no delete. Worker rows are kept so a build from two years ago still
shows which machine produced it, with what architectures and what version.
Revoked workers move behind a **Show retired** toggle so the list stays useful.

A retired machine that comes back re-enrolls and appears as revoked with a
recent "last seen", which is the signal to approve it again if you want it back.
Its certificate is still on file, so that is one click.

## How much memory did a build need?

The Builds list has a **Peak RAM** column: the high-water mark of the build's
whole process tree — `makechrootpkg`, `systemd-nspawn`, `makepkg`, and one
compiler per core, measured together.

It is most useful on a build that failed with exit 137, which is an OOM kill and
says nothing else about itself. Knowing the last successful build of the same
package peaked at 6 GiB turns that into a number you can act on.

The figure is exact, not sampled: each build runs in a cgroup of its own and
this is that cgroup's `memory.peak`, so it covers every process in the tree with
no polling and no blind spot.

**A dash means not reported**, not zero. That happens when:

- the worker predates this, or is the deprecated container builder — Docker
  exposes no peak figure on cgroup v2, where `max_usage` no longer exists;
- the worker could not prepare a cgroup subtree. A container needs
  `privileged`, which `mkarchroot` and `arch-nspawn` already require, so the
  published images qualify. A native install needs `Delegate=yes` on the unit,
  which `aurcache-worker.service` sets; systemd otherwise owns that subtree.
- the kernel is older than 5.19, which is where `memory.peak` arrived.

Builds run either way — this measures the work, it does not do it.

## Why is a build not starting?

`aurcache-cli doctor` answers this directly. It walks the chain — server, token,
fleet, queue — and names the first thing that is actually wrong, with the
command that fixes it:

```
$ aurcache-cli doctor
✓ server   reachable at http://localhost:8080/api
✓ token    authenticated as you
✗ workers  1 worker(s) enrolled, none approved
           → aurcache-cli worker approve 3
✓ queue    nothing queued
```

It tells the three fleet failures apart, because they are three different
mistakes: nothing enrolled, enrolled but never approved, and approved but not
calling in. The queue check reports the same reasons the Builds page shows,
[described below](#what-the-builds-page-is-telling-you).

`doctor` exits non-zero if any check fails, so it doubles as a health gate in a
script, and `--format json` gives the checks as structured output.

`aurcache-cli builds watch` follows the queue and explains what it sees:

```
$ aurcache-cli builds watch
[   0s] turso #3: waiting-for-deps
[   0s] libaegis #1: active
[  47s] libaegis #1: successful
[  95s] simsimd #2: successful
[  96s] turso #3: active
```

It reports changes rather than repeating the current state, prints a heartbeat
while work is in flight, and **fails fast on a stall**: if nothing has changed
for a while and nothing is building, the queue cannot advance on its own, so it
says so instead of waiting out the timeout.

`--package NAME` narrows it to one package; `--timeout` and `--stall-after`
adjust the limits.

## What the Builds page is telling you

An enqueued build that no worker can claim is flagged with the reason:

| Reason | Meaning | What to do |
|---|---|---|
| Reserved for *worker* (offline) | Package affinity ties it to a worker that is not online | Start that worker, or revoke it to release the reservation |
| No worker builds *arch* | No approved worker handles that architecture | Enroll one, or remove the architecture from the package |
| All capable workers are offline | Workers exist for the job but none is online | Start one |

A build without a reason is simply queued behind other work — that is normal and
needs nothing.

## Builds that never finish

A worker sends a heartbeat every `WORKER_HEARTBEAT_INTERVAL` seconds. If one
stops, its builds are requeued after `LEASE_TTL` and retried, up to
`MAX_ATTEMPTS` times before being failed for good. A build that runs
implausibly long is reclaimed the same way even if its worker is still
heartbeating, which covers a hung build on a healthy machine.

If a build is genuinely just slow, raise `WORKER_BUILD_TIMEOUT` on the worker —
the default kills a build after three hours.
