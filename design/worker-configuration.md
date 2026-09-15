# Configuring workers from the server

How a worker's settings could be seen and changed through AURCache's API and UI,
instead of only through each machine's environment, and how the change would
reach a running worker.

Status: **proposal**. Nothing here is implemented. It compares options and
recommends a first step; the transport question in particular is meant to be
decided before any of it is built.

---

## Where things stand

A worker is configured entirely by its environment: `/etc/aurcache/worker.env`
on a native install, the container's environment in the images. About 45
variables, parsed at startup (`aurcache_worker_core::config`,
`aurcache_worker::config`). Changing one means a shell on that machine, an edit,
and a restart -- which kills any build in progress, because builds live in the
worker's cgroup.

The server sees part of it. Registration, which a worker repeats on every start,
carries its name, architectures, concurrency, priority and package affinity
(`RegisterRequest`), and the Workers page shows those. Everything else --
limits, cache budgets, timeouts, intervals -- is invisible to the server.

The server can reach a worker only by answering it. The worker is pull-only:
it claims jobs, posts logs and posts a heartbeat every `WORKER_HEARTBEAT_INTERVAL`
(15 s), and `HeartbeatResponse { cancel }` is the one channel back. A Stop takes
up to a heartbeat interval to arrive, plus the build loop's 5 s tick.

### What that has cost recently

- Raising `unreal-engine`'s build-tree budget to `WORKER_BUILDDIR_MAX_BYTES=450G`
  would have been silently ignored: the variable took bytes only, and a value
  that did not parse fell back to the 200 GiB default. It was caught by reading
  the parser, not by anything the worker showed. Parsing failures now warn, but
  a warning in a worker's journal is still a place nobody looks.
- CPU and memory limits existed as *server* settings, `cpu_limit` and
  `memory_limit`, that nothing read after builds moved to workers. They have
  been removed in favour of `WORKER_BUILD_MEMORY_MAX`/`WORKER_BUILD_CPUS`.
  `max_concurrent_builds` and `builder_image` are dead the same way and are
  still on the settings API.
- Giving `unreal-engine` a bigger build-tree budget meant an SSH session to the
  one worker allowed to build it. A fleet of five is five edits.

The last two point at the same thing: a setting that belongs to the worker has
nowhere on the server to live, so it was either put in the wrong place or left
unreachable.

---

## Two separate questions

"Configure workers from the server" is two decisions that are easy to conflate:

1. **Where the value lives and who wins** -- the worker's environment, the
   server's database, or both with a precedence.
2. **How a change reaches a running worker** -- at the next start, on the next
   heartbeat, or pushed the moment it is saved.

The first decides what the UI can promise; the second only how soon. They are
treated in that order.

---

## 1. Which settings the server may set

Not all of them. Some are facts about the machine, some are the machine's
security boundary, and some are policy that is genuinely better set in one
place.

| Class | Examples | Server may set? |
|---|---|---|
| **Bootstrap** -- needed before the server can be reached | `AURCACHE_URL`, `AURCACHE_SERVER_CA_FINGERPRINT`, `AURCACHE_ENROLLMENT_TOKEN`, `WORKER_DATA_DIR` | **No.** Circular. |
| **Host access** -- paths, users, binaries, mounts | `WORKER_CHROOT_DIR`, `WORKER_CACHE_DIR`, `WORKER_BUILD_USER`, `WORKER_BIND_MOUNTS`, `WORKER_MAKECHROOTPKG`, `WORKER_GIT_SSH_KEY` | **No.** See below. |
| **Machine facts** | `WORKER_ARCHES`, `WORKER_EMULATED_ARCHES`, `WORKER_NAME` | No -- reported, not chosen. |
| **Policy and tuning** | `WORKER_CONCURRENCY`, `WORKER_PRIORITY`, `WORKER_PACKAGES`, `WORKER_BUILD_MEMORY_MAX`/`_SWAP_MAX`/`_CPUS`, `WORKER_BUILD_TIMEOUT`, `WORKER_BUILDDIR_MAX_BYTES`/`_MIN_FREE`, cache budgets and TTLs, `WORKER_CHROOT_REFRESH_INTERVAL`, mirrorlist | **Yes.** |

**Why host access must stay local.** The worker runs devtools as root
(`aurcache ALL=(ALL) NOPASSWD: ALL`, with the reasoning in
`packaging/aurcache-worker.sudoers`), and the isolation that constrains
untrusted input is the chroot and the separate build user. Today a compromised
server can make a worker build anything, but that code runs as `builder` inside
a chroot. A server that could set `WORKER_BIND_MOUNTS` could bind `/` into the
chroot; one that could set `WORKER_BUILD_USER` or `WORKER_MAKECHROOTPKG` could
run a build as root outright. Keeping those keys off the wire keeps a server
compromise a build compromise rather than root on every worker.

So the design is an **allowlist**: a `WorkerSetting` enum in `aurcache-common`,
like `Setting` for the server, naming exactly the keys the server may send. A
key not on it is ignored by the worker however it arrives.

### Precedence

Three candidates:

| Model | Behaviour | Against it |
|---|---|---|
| **Server wins** | The UI is always right about what a worker does | A machine cannot pin its own safety limit; a mistaken fleet-wide change applies everywhere with no local veto |
| **Environment wins** (server as default) | A local value is a deliberate pin; the server fills in what is unset | An env-pinned key cannot be changed from the UI -- which the UI must then say |
| **Per-key lock** | Each key says which side wins | A policy knob per key; nobody has asked for it |

**Recommend: environment wins.** It is the model the server's own settings
already use (`Package -> Env -> Global -> Default`, where Env beats the
database), it is how the mirrorlist override already works
(`design/mirrorlist-configuration.md`), and it means deploying this changes
nothing for an existing worker: every value it has today is set in its
environment and keeps winning. The settings page already renders the
consequence -- an env-pinned row is read-only and names the variable -- and a
worker page can reuse that component.

Resolution per key, highest first:

1. the worker's environment,
2. a value set for **this worker** on the server,
3. a **fleet default** set on the server,
4. the built-in default.

Storage mirrors per-package settings: a `worker_settings (worker_id NULL for
the fleet default, key, value)` table, the fleet default the way the global
settings row is.

### Validation

Validated twice with the same code. The server parses a value before storing it
(so the UI rejects `1.5G` at save time rather than a worker ignoring it later);
the worker parses what arrives with the same `parse_size`/`parse_duration`, and
a value that does not parse is treated as unset, as a bad environment value is.
The parsers live in `aurcache-worker-core` today, which the server should not
depend on (it brings the worker's HTTP client); they have no dependencies of
their own and move to `aurcache-common`.

### Visibility is worth having on its own

Independent of editing: the worker reports its **effective** configuration, with
the source of each value, on registration and whenever it changes. The UI
shows every key and where it came from. That alone would have shown a
`450G` that meant 200 GiB, and it costs one field on an existing message.

---

## 2. How a change reaches the worker

What actually needs to be timely:

| Message | Direction | Latency that matters |
|---|---|---|
| Stop a build | server -> worker | seconds (an operator is watching) |
| Configuration changed | server -> worker | tens of seconds is fine |
| Pause / drain / resume | server -> worker | seconds |
| Leases, liveness | worker -> server | the heartbeat interval, by design |
| Logs | worker -> server | already streamed by POST |

Only Stop is latency-sensitive, and it already works through the heartbeat.

### Options

**A. Registration only.** The server returns the worker's configuration in the
registration response; a change applies at the next start. The server can
prompt a restart (a flag in `HeartbeatResponse`) that the worker honours once
idle. Smallest change, no new transport. But "apply" means "restart when idle",
which on a worker building `unreal-engine` is hours away.

**B. Heartbeat piggyback.** The heartbeat carries the `config_version` the
worker has applied; when the server's differs, `HeartbeatResponse` includes the
new configuration. Latency is one heartbeat interval. No new endpoint, no new
connection, nothing new for a proxy to pass; it is exactly how cancel already
travels, with the same mTLS identity and the same failure behaviour -- if the
server is unreachable, nothing changes.

**C. Long-poll.** `GET /worker/events?since=<cursor>` held open until there is
something to say, or for 30-60 s. Near-instant, plain HTTP, and proxies tolerate
it. But it is a second connection per worker, and a cursor to keep consistent
across server restarts.

**D. Server-sent events.** `GET /worker/events` as an event stream: server to
worker only, worker to server stays on POST. Rocket has it built in
(`rocket::response::stream::EventStream`, no new dependency), and reqwest reads
a streaming body, so the worker needs no new dependency either. Reconnects are
the client's job, and the stream is one-way, which is all that is missing today.

**E. WebSocket.** One persistent, bidirectional, mTLS connection per worker;
heartbeat, cancel, configuration and possibly logs all move onto it. Instant in
both directions. New dependencies on both sides (`rocket_ws`,
`tokio-tungstenite` with a rustls client certificate), and a new failure model:
half-open connections need pings, reconnects need backoff, messages need
versioning, and a reverse proxy must pass the upgrade. The server has to track
which process holds which worker's socket, which a single server does in memory
and several replicas cannot.

**F. gRPC bidirectional streaming** (tonic). Everything E offers with generated
types. A second RPC stack beside Rocket and a new protocol for anyone
debugging a worker; far more than this needs.

### Compared

| | A. Registration | B. Heartbeat | C. Long-poll | D. SSE | E. WebSocket | F. gRPC |
|---|---|---|---|---|---|---|
| Config latency | next restart | ≤ 15 s | ~instant | ~instant | ~instant | ~instant |
| Stop latency | as today | as today | ~instant | ~instant | ~instant | ~instant |
| New dependencies | none | none | none | none | 2 | several |
| New endpoints | 0 | 0 | 1 | 1 | 1 (upgrade) | a service |
| Connections per worker | as today | as today | +1 held | +1 held | 1, persistent | 1, persistent |
| Proxy concerns | none | none | idle timeout | idle timeout, buffering | upgrade passthrough | HTTP/2 end to end |
| If the channel drops | n/a | nothing changes | reconnect | reconnect | reconnect; leases still need a fallback | as E |
| Server replicas | fine | fine | fine | sticky | sticky | sticky |

The row that settles E and F: **leases cannot move onto a push channel.** A
lease exists to decide a worker's fate when it has *stopped* talking, so it must
be renewed by the worker and expire on the server regardless of any connection.
A socket that carries heartbeats still needs lease expiry behind it, so E does
not remove the heartbeat; it adds a second path beside it.

---

## Recommendation

**Phase 1 -- visibility, then configuration over the heartbeat (B).**

1. `WorkerSetting` allowlist and `WorkerConfig` document in `aurcache-common`,
   with the per-key parsers the environment already uses.
2. The worker reports its effective configuration and each value's source on
   registration and in the heartbeat when it changes (a version keeps it off
   the wire otherwise). The Workers page shows it. Useful on its own; ship it
   first.
3. `worker_settings` table; API to set a per-worker value and a fleet default,
   with the same validation.
4. `Heartbeat.config_version` / `HeartbeatResponse.config`; the worker resolves
   env > worker > fleet > default and applies the result.
5. UI: a worker detail page with the effective table (env-pinned rows read-only,
   as on the settings page) and editable overrides; fleet defaults in a Workers
   section of Settings.
6. Remove the dead server settings `max_concurrent_builds` and `builder_image`
   the way `cpu_limit`/`memory_limit` were.

**Phase 2, only if latency is felt -- SSE (D) for server-to-worker events.** Stop
and "configuration changed" become events; the heartbeat stays exactly as it is
for leases, and is the fallback if the stream is down, so correctness never
depends on the stream. D over C for the lower bookkeeping; D over E because it
adds the missing direction without a second protocol for the direction that
already works.

**Not recommended now: WebSocket or gRPC.** Revisit if the worker-to-server
direction becomes a problem too -- log streaming at a scale where one POST per
batch hurts -- since that, not configuration, is what would justify a
bidirectional connection.

---

## Applying a change on a running worker

Not every key can change under a running worker, and the document should say
which:

| Takes effect | Keys |
|---|---|
| **Next build** | build limits, build timeout, builddir budget and floor, cache budgets and TTLs, mirrorlist |
| **Immediately** | chroot refresh interval; concurrency (the claim loop's semaphore is sized once at startup today, so it would add or forget permits -- lowering it lets running builds finish rather than killing any) |
| **Next registration** | priority, package affinity (the server holds these for scheduling, so the worker re-registers when they change) |

A build keeps the limits it started with. Changing a limit mid-build could be
done -- they are cgroup files -- but a build that was sized for one limit and
killed by another is a worse outcome than waiting for the next one.

Nothing on the allowlist needs a restart. That is part of why paths and users
are not on it.

---

## Open questions

1. **Package affinity from the server.** `WORKER_PACKAGES` is policy, but
   `design/worker-routing.md` chose worker-declared affinity deliberately: the
   capability (a key, a toolchain) is on the machine. Should the server be
   allowed to add a package to a worker's list, or only to show it?
2. **Audit.** Configuration changes should appear in the activity log, as package
   changes do. Per worker or per key?
3. **Drain/pause.** Worth adding to the allowlist in phase 1 as a boolean, or a
   separate operator action?
4. **The legacy container worker.** `aurcache-worker-docker` reads different
   variables (`CPU_LIMIT` in milli-CPUs, `MEMORY_LIMIT` in MB). Leave it
   env-only until it is removed, or map the allowlist onto it?
