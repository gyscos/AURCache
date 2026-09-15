# Configuring workers from the server

How a worker's settings could be seen and changed through AURCache's API and UI,
instead of only through each machine's environment, and how the change would
reach a running worker.

Status: **proposal, revised after review**. Nothing here is implemented. Two
reviews ([`worker-configuration-review.md`](worker-configuration-review.md),
[`worker-configuration-review-codex.md`](worker-configuration-review-codex.md))
agreed with the security boundary, the precedence model and the transport, and
found the protocol under-specified. This revision adopts most of what they
raised; [Review outcomes](#review-outcomes) lists what changed, what did not,
and the few review claims that do not match the code.

---

## Where things stand

A worker is configured entirely by its environment: `/etc/aurcache/worker.env`
on a native install, the container's environment in the images. About 45
variables, parsed at startup (`aurcache_worker_core::config`,
`aurcache_worker::config`). Changing one means a shell on that machine, an edit,
and a restart -- which kills any build in progress, because builds live in the
worker's cgroup.

The server sees part of it. Registration, which a worker performs once per
process start, carries its name, architectures, concurrency, priority and
package affinity (`RegisterRequest`), and the Workers page shows those.
Everything else -- limits, cache budgets, timeouts, intervals -- is invisible to
the server.

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
treated in that order, followed by the protocol that carries a change and what
a running worker does with it.

---

## 1. Which settings the server may set

Not all of them. Some are facts about the machine, some are the machine's
security boundary, and some are policy that is genuinely better set in one
place.

| Class | Keys | Server may set? |
|---|---|---|
| **Bootstrap** -- needed before the server can be reached | `AURCACHE_URL`, `AURCACHE_SERVER_CA_FINGERPRINT`, `AURCACHE_ENROLLMENT_TOKEN`, `AURCACHE_ENROLLMENT_DIR`, `WORKER_DATA_DIR` | **No.** Circular. |
| **Local network view** | `AURCACHE_REPO_HOST`, `AURCACHE_REPO_URL` | **No.** How *this* machine reaches the repository (split DNS, a VPN, a proxy); the server's view of its own address is exactly what these exist to override. |
| **Protocol timing** | `WORKER_HEARTBEAT_INTERVAL` | **No.** It has to stay well inside the server's `LEASE_TTL` and `WORKER_LIVENESS_TIMEOUT` (both 60 s); a server-set interval that drifted past them would get running builds reaped. |
| **Host access** -- paths, users, binaries, mounts | `WORKER_CHROOT_DIR`, `WORKER_CACHE_DIR`, `WORKER_BUILD_USER`, `WORKER_BIND_MOUNTS`, `WORKER_MAKECHROOTPKG`, `WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS`, `AURCACHE_NSPAWN_KEEP_UNIT`, `AURCACHE_DROPIN` | **No.** See below. |
| **Machine facts** | `WORKER_ARCHES`, `WORKER_EMULATED_ARCHES`, `WORKER_NAME`, `WORKER_CHROOT_OVERLAY` | No -- reported, not chosen. The chroot mode depends on what the filesystem can do. |
| **Policy and tuning** | `WORKER_CONCURRENCY`, `WORKER_PRIORITY`, `WORKER_PACKAGES` (per worker only, see below), `WORKER_BUILD_MEMORY_MAX`/`_SWAP_MAX`/`_CPUS`, `WORKER_BUILD_TIMEOUT`, `WORKER_BUILDDIR_MAX_BYTES`/`_MIN_FREE`, `WORKER_CACHE_MAX_SIZE`/`_TTL`, `WORKER_PKGCACHE_MAX_SIZE`/`_TTL`, `WORKER_SRCCACHE_MAX_SIZE`, `WORKER_CHROOT_REFRESH_INTERVAL`, `WORKER_POLL_INTERVAL`, `WORKER_KEYSERVER` | **Yes.** |

Two keys the first draft listed are no longer here:

- **Mirrorlist.** The server already delivers a per-architecture mirrorlist at
  registration (`design/mirrorlist-configuration.md`), and a worker's
  `WORKER_MIRRORLIST_SERVERS`/`_FILE` overrides it locally. A single worker-level
  mirrorlist setting would be a third, architecture-blind mechanism beside those.
  If a server-side mirrorlist change should reach running workers, it rides the
  same configuration snapshot below as a separate field, keyed by architecture.
- **Drain/pause.** A lifecycle state, not configuration; see
  [Decisions](#decisions-on-the-open-questions).

`WORKER_KEYSERVER` is safe to set centrally: signature checks are pinned by the
PKGBUILD's `validpgpkeys`, so a keyserver can withhold a key but not substitute
one.

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
key not on it is refused by the worker however it arrives, and reported as
unsupported (below).

### Precedence

Three candidates:

| Model | Behaviour | Against it |
|---|---|---|
| **Server wins** | The UI is always right about what a worker does | A machine cannot pin its own safety limit; a mistaken fleet-wide change applies everywhere with no local veto |
| **Environment wins** (server as default) | A local value is a deliberate pin; the server fills in what is unset | An env-pinned key cannot be changed from the UI -- which the UI must then say |
| **Per-key lock** | Each key says which side wins | A policy knob per key; nobody has asked for it |

**Recommend: environment wins.** It is the model the server's own settings
already use (`Package -> Env -> Global -> Default`, where Env beats the
database), it is how the mirrorlist override already works, and it means
deploying this changes nothing for an existing worker.

That last property cuts both ways, as both reviews pointed out: most deployed
workers already set their policy in the environment, so on day one most rows
are pinned and the UI cannot change them. That is the right default -- a pin
is a deliberate local veto -- but it needs an adoption path, not just a
read-only row:

- **The UI names the pin and the way out**: "Pinned by `WORKER_CONCURRENCY` on
  this worker; remove it from the worker's environment to manage it here." A
  server value saved for a pinned key is stored, and shown as *not in effect*,
  never as a change that took.
- **Shipped defaults stop pinning policy.** `docker-compose.remote-worker.yaml`
  sets `WORKER_CONCURRENCY=2`, and `aurcache-cli setup compose`/`setup worker`
  write `WORKER_CONCURRENCY` when given. Templates keep bootstrap and host-access
  variables set and leave policy commented out, so a new worker is manageable
  from the server unless someone chooses otherwise.
- **The documented adoption workflow**: keep bootstrap, network and host-access
  variables local; move policy values to the server (fleet default or per
  worker); remove them from the worker's environment; restart once.

### Who resolves which layer

The first draft had the worker resolve all four layers. The server owns two of
them, so it resolves those:

- **Server**: `snapshot[key] = per_worker[key] or fleet_default[key]`, for every
  allowlisted key that has either. The snapshot is a flat map; it never
  mentions "fleet" or "per worker".
- **Worker**: `effective[key] = environment[key] or snapshot[key] or built_in_default[key]`.

This keeps the database model off the wire, keeps the payload small, and gives
the snapshot a single meaning, which is what makes its revision unambiguous
(below).

**Package affinity has no fleet default.** A fleet-wide `packages` value would
give every otherwise-unset worker the same reservation, which is the opposite of
affinity. The API refuses `packages` at fleet scope.

### Storage

`worker_settings (worker_id NULL for the fleet default, key, value)`, the fleet
default the way the global settings row is, with:

- **Two partial unique indexes**, because `UNIQUE (worker_id, key)` does not stop
  duplicate fleet rows: NULLs are distinct in a unique constraint on SQLite and
  on Postgres alike.
  ```sql
  CREATE UNIQUE INDEX idx_worker_settings_fleet  ON worker_settings (key)            WHERE worker_id IS NULL;
  CREATE UNIQUE INDEX idx_worker_settings_worker ON worker_settings (worker_id, key) WHERE worker_id IS NOT NULL;
  ```
- **`worker_id` references `workers(id) ON DELETE CASCADE`**; foreign keys are
  enforced on both backends.
- **Dump and restore** carry the table. Fleet rows restore as they are;
  per-worker rows are remapped by certificate fingerprint, the way
  `restore.rs` already matches workers, and dropped with the worker when its
  fingerprint is not restored.

### Validation

Validated on both sides with the same code. The server parses a value before
storing it, so the UI rejects `1.5G` at save time rather than a worker
discarding it later. The parsers already live in `aurcache-common::units`.

The worker can still refuse a value the server accepted: a key newer than the
worker binary, a CPU limit on a host whose cgroup `cpu` controller cannot be
enabled, a memory limit above what the machine has. The first draft said such a
value is "treated as unset", which for a safety limit means falling back to
*unlimited* while the server shows the limit as set. Instead:

- A refused value **keeps the worker's previous usable value** for that key: the
  last one it applied from a snapshot, or its environment/default if it never
  had one. A limit is never loosened by a rejection.
- The worker reports **per-key status** for every key it received: `applied`,
  `overridden` (env-pinned), `unsupported` (not on this binary's allowlist), or
  `rejected` with a short, operator-readable reason.
- The UI flags a worker with a rejected or unsupported key, rather than showing
  the saved value as in effect.

### Saving is one transaction

An operator often changes related keys together -- lower concurrency with a
higher per-build memory limit, say. Written row by row, a heartbeat between
two writes could deliver a combination nobody chose. So a save is one
`PATCH` of a set of keys at one scope:

1. validate every key, and any cross-key rule, before writing anything;
2. insert, update and delete the rows in one transaction;
3. write one activity entry for the save;
4. the next snapshot computed for an affected worker includes all of it or none.

Because the revision is derived from the snapshot's content (below), step 4
needs no revision counter to bump: a worker cannot observe a half-written save
because the snapshot is only ever read from committed rows.

### Visibility is worth having on its own

Independent of editing: the worker reports its **effective** configuration,
with the source and status of each value. The UI shows every key and where it
came from. That alone would have shown a `450G` that meant 200 GiB.

---

## 2. How a change reaches the worker

What actually needs to be timely:

| Message | Direction | Latency that matters |
|---|---|---|
| Stop a build | server -> worker | seconds (an operator is watching) |
| Configuration changed | server -> worker | tens of seconds is fine |
| Drain / resume | server only | immediate, and needs no message (see Decisions) |
| Leases, liveness | worker -> server | the heartbeat interval, by design |
| Logs | worker -> server | already streamed by POST |

Only Stop is latency-sensitive, and it already works through the heartbeat.

### Options

**A. Registration only.** The server returns the worker's configuration in the
registration response; a change applies at the next start. The server can
prompt a restart (a flag in `HeartbeatResponse`) that the worker honours once
idle. Smallest change, no new transport. But "apply" means "restart when idle",
which on a worker building `unreal-engine` is hours away.

**B. Heartbeat piggyback.** The heartbeat carries the revision of the last
snapshot the worker received; when the server's differs, `HeartbeatResponse`
includes the new snapshot. Latency is one heartbeat interval. No new endpoint,
no new connection, nothing new for a proxy to pass; it is exactly how cancel
already travels, with the same mTLS identity and the same failure behaviour --
if the server is unreachable, nothing changes.

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

**Recommended: B**, with the snapshot also returned at registration (next
section), so a worker that restarts has its server configuration before it
claims anything rather than one heartbeat later.

---

## 3. The protocol

### Delivery and effect are separate

The first draft had one `config_version` that the worker reported as "applied".
With environment-wins that cannot work: a worker that receives revision 12 with
an env-pinned key has correctly *received* 12 but deliberately does not run it,
and a server comparing one version against its desired state would resend the
same snapshot on every heartbeat forever. So there are two things:

- **`received_revision`** -- the revision of the last snapshot the worker parsed
  and accepted (every key given a status). It only controls retransmission.
- **`effective`** -- the worker's resolved configuration: per key, the value,
  its source (`env`, `server`, `default`) and its status. It controls what the
  UI shows and the scheduling fields (below).

The **revision** is the SHA-256 of the canonical serialization of the snapshot
the server would send that worker: sorted keys, normalized values. It changes
exactly when that worker's snapshot changes -- a fleet default edit changes it
for every worker without a per-worker override of that key and for no other --
with no counter to keep consistent across restarts or replicas. It is a
revision of the server's payload, never a hash of the worker's env-resolved
result.

### Messages

```text
RegisterRequest   { ...as today, config_protocol: 1 }
RegisterResponse  { ...as today, config: Option<ConfigSnapshot> }

Heartbeat         { ...as today, received_revision: Option<String>,
                    effective: Option<EffectiveConfig> }      // only when it changed
HeartbeatResponse { cancel: Vec<i32>, config: Option<ConfigSnapshot> }   // #[serde(default)]

ConfigSnapshot    { revision: String, settings: BTreeMap<String, String> }
EffectiveConfig   { received_revision: Option<String>,
                    settings: BTreeMap<String, EffectiveSetting> }
EffectiveSetting  { value: Option<String>, source: Env | Server | Default,
                    status: Applied | Overridden | Unsupported | Rejected,
                    reason: Option<String> }
```

- **A snapshot is always complete**, never a delta, so a worker that missed
  heartbeats needs nothing it did not receive.
- **Settings travel as a string map**, not as a `WorkerSetting`-keyed struct. A
  key this worker does not know is reported `unsupported`; it cannot fail
  deserialization.
- **Old workers keep working.** `parse_heartbeat_response` today falls back to an
  empty response when the body does not deserialize -- which would also drop
  `cancel`. So `config` is optional with `#[serde(default)]`, and the server only
  ever sends it to a worker that declared `config_protocol` at registration. A
  worker that never declared it is shown as "configuration not supported by this
  worker version", and its saved overrides as not delivered.
- **Unreachable server**: nothing changes, as with cancel today. A worker keeps
  its last snapshot until it receives a newer one; a restarted worker gets one at
  registration before claiming.

### Sequence

```text
Worker                               Server
  |-- register {config_protocol:1} ->  |
  |<- {config: rev a1b2, settings} --  |   worker applies, reports statuses
  |-- heartbeat {received a1b2,        |
  |    effective{...}} -------------->  |   stores effective config
  |<- {cancel:[]} -------------------  |
  |                                    |   operator saves: one transaction
  |-- heartbeat {received a1b2} ----->  |   worker's snapshot now hashes to c3d4
  |<- {cancel:[], config: rev c3d4} --  |
  |-- heartbeat {received c3d4,        |
  |    effective{...}} -------------->  |   UI shows what took, what was pinned
  |<- {cancel:[]} -------------------  |
```

---

## 4. Applying a change on a running worker

### Snapshots per job

A job reads the runtime configuration once, when it starts, and keeps that copy
for its lifetime; a later snapshot is seen whole by the next job. No job ever
runs with half of one save and half of another, and a build keeps the limits it
started with -- a build sized for one limit and killed by another is a worse
outcome than waiting for the next one.

| Takes effect | Keys |
|---|---|
| **Next job** | build limits, build timeout, builddir budget and floor, cache budgets and TTLs, keyserver |
| **Next loop iteration** | chroot refresh interval, poll interval |
| **Through the concurrency gate** | concurrency (below) |
| **On the server, at once** | priority, package affinity (below) |

Nothing on the allowlist needs a restart. That is part of why paths and users
are not on it.

### Concurrency

The claim loop holds a `tokio::sync::Semaphore` sized once at startup
(`runner.rs`). Resizing it in place does not work for lowering: while every
permit is held, `forget_permits` removes none, and each finishing build then
returns its permit and restores the old capacity. So the semaphore is replaced by
a small **concurrency gate**: a target, a running count, and a deficit -- a
permit returned while running is above the target is absorbed instead of
released. Lowering lets running builds finish; raising releases permits at once.

The server's claim query already refuses a worker whose `active` has reached
`workers.concurrency`, so lowering is enforced on the server side immediately
too. Raising is not: if the server raised `workers.concurrency` before the
worker's gate had, it would offer jobs the worker cannot start. So
`workers.concurrency` is set from the concurrency the worker **reports as
effective**, never from the saved value.

### Priority and package affinity

These are consumed only by the server's claim logic; the worker never uses
them. The first draft had the worker re-register to deliver them, which is
circular -- a running worker registers once per process -- and would bring
back the restart this design exists to avoid. Instead the server updates
`workers.priority` and `workers.package_affinity` itself as soon as it knows
the effective value: the saved server value when the worker has reported the
key as not env-pinned, the worker's own value when it is pinned. Registration
keeps reporting the worker's environment values, and stops overwriting a
server-managed value that the environment does not pin.

### CPU limits need the controller first

The worker writes `+cpu` to `cgroup.subtree_control` once, at startup, and only
if `WORKER_BUILD_CPUS` was set. A CPU limit that arrives later would make every
following build fail writing `cpu.max`. So `Hierarchy::for_build` enables the
controllers the job's limits need, on demand, and verifies them; if that fails
the CPU limit is `rejected` with the reason and the previous value kept, rather
than failing builds.

---

## Recommendation

**Phase 1 -- visibility, then configuration over the heartbeat (B).**

1. `WorkerSetting` allowlist in `aurcache-common`, with the per-key parsers the
   environment already uses. Retire the dead server settings
   `max_concurrent_builds` and `builder_image` the way `cpu_limit`/`memory_limit`
   were.
2. **Read-only first.** The worker reports `effective` (value, source, status)
   at registration and in the heartbeat when it changes; the server stores it and
   the Workers page shows it, with parse errors flagged. Useful on its own; ship
   it first.
3. `worker_settings` table with the partial indexes, cascade and dump/restore;
   transactional `PATCH` for a worker and for the fleet default; one activity
   entry per save.
4. Snapshot delivery: `config_protocol` at registration, `config` in the
   registration and heartbeat responses, `received_revision` back.
5. Worker application: per-job snapshots, the concurrency gate, on-demand cgroup
   controllers, per-key status with rejected values keeping the previous one.
   Server scheduling fields follow the reported effective values.
6. UI: a worker detail page with the effective table (env-pinned rows name their
   variable and the way to unpin them) and editable overrides; fleet defaults in
   a Workers section of Settings. Policy variables commented out of the shipped
   compose files and the CLI's generated ones.

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

## Decisions on the open questions

The first draft left four questions open. Both reviews answered them the same
way, and this revision adopts those answers.

1. **Package affinity from the server: yes, per worker only.** `design/worker-routing.md`
   made affinity worker-declared because the capability (a key, a toolchain) is
   on the machine. The server may now set it for one worker -- an operator who
   has just provisioned a credential should not need a restart -- but not as a
   fleet default, which would defeat the reservation. The editor says that the
   worker must actually have what those packages need.
2. **Audit: one entry per save**, `WorkerConfigUpdated { worker: Option<name>,
   changed_keys }`, with `None` for the fleet default. One entry per key would
   flood the log whenever a form saves several fields.
3. **Drain: a server-side lifecycle state, not a setting.** A `draining` column
   on `workers`, set by an operator action. The claim query stops offering that
   worker jobs at once; running builds finish; the worker needs no message and
   no new code. As a setting it would be wrong twice over: a worker could boot
   drained from its environment, and a fleet default of `true` would stop the
   whole fleet.
4. **The legacy container worker stays env-only.** `aurcache-worker-docker`
   reads different variables in different units (`CPU_LIMIT` in milli-CPUs,
   `MEMORY_LIMIT` in MB) and is on its way out; mapping the allowlist onto it
   would be a second, barely tested implementation.

---

## Review outcomes

What changed from the first draft because of the reviews:

| Raised | By | Outcome |
|---|---|---|
| One `config_version` cannot mean both received and effective | codex | Adopted: `received_revision` and `effective` are separate |
| Revision as a content hash of the server's payload | both | Adopted |
| Saves must be atomic; snapshots complete, not deltas | codex | Adopted |
| Rejected values must not fall back to a weaker default; per-key status | both | Adopted, with the previous usable value kept |
| Server resolves fleet vs per-worker; worker only env > snapshot > default | both | Adopted |
| Priority/affinity must not wait for re-registration | both | Adopted: server updates scheduling fields directly |
| Semaphore cannot be resized in place; schedule from reported capacity | both | Adopted: concurrency gate |
| CPU controller only enabled at startup | both | Adopted: enabled on demand, rejection on failure |
| Forward compatibility of `HeartbeatResponse` | both | Adopted, plus a `config_protocol` capability so old workers are never counted as configured |
| Env-wins leaves existing workers unmanageable | both | Kept env-wins; added the adoption path and template changes |
| Partial unique indexes, cascade, dump/restore | both | Adopted |
| Allowlist gaps (repo host/URL, keyserver, overlay, heartbeat interval, poll interval) | review | Adopted as classified in the review |
| Mirrorlist as a worker setting conflicts with per-arch mirrorlists | review | Adopted: removed from the allowlist |
| Open questions (affinity, audit, drain, legacy worker) | both | Adopted as answered |

Added in this revision beyond the reviews: the snapshot is also returned at
registration, so a restarted worker has its configuration before its first
claim; and registration stops overwriting server-managed routing fields that
the environment does not pin.

Where the reviews do not match the code, for the record -- none of these
change a conclusion:

- `Semaphore::forget_permits` does not panic when no permits are available; it
  returns how many it removed, which is none. The point that returned permits
  restore the old capacity stands.
- The existing mirrorlist mechanism delivers the mirrorlist at registration, not
  per build.
- Duplicate `NULL` rows under a plain unique constraint are not specific to
  SQLite; Postgres allows them too (without `NULLS NOT DISTINCT`), so the partial
  indexes are needed on both backends.
