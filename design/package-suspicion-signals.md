# Design: Flagging suspicious packages (recipe and artifact signals)

AURCache builds untrusted PKGBUILDs in a sandbox that contains what they do, but
containment is not the same as knowledge. This design adds a **report-only
suspicion signal** for every build, from two spots that *existing* AUR security
tools cannot both reach:

1. **The recipe** — a static scan of the exact PKGBUILD tree a worker is about
   to build, run pre-build on the worker.
2. **The finished artifact** — a scan of the built `.pkg.tar.zst` itself, run at
   publish on the server, where AURCache is one of the very few places on earth
   that holds the final package before anything installs it.

Both are counters and rule hits, never verdicts and never gates. They surface as
a persisted JSON blob on the build row and a chip in the UI, and they **fail no
build and publish no gate by default** — the same "report is observation, not
interruption" posture as `design/sandbox-attempt-awareness.md`.

Status: **Proposed** · Last updated: 2026-09-15

The ecosystem survey that shaped this, summarized here rather than left to the
conversation it came from:

- **Recipe analysis is a mature market.** `ks-aur-scanner` (Rust, 110+ rule
  codes, campaign-trained, JSON/SARIF output), `traur` (Rust, broadest signal
  set: PKGBUILD + `.install`, typosquatting / orphan-takeover / git-history
  provenance), `aurscan` (static rules + LLM verdict, `mvdan.cc/sh`-based
  deobfuscation), `archlinux-inputs-fsck` (kpcyrd — but it *sources* the
  PKGBUILD to load it, and is therefore a job for AURCache's already-sandboxed
  parse bridge, not for the worker's static pass), `pkglint`, `namcap`,
  `arch-toolkit`. We take one of these as a subprocess, not by linking.
- **Artifact analysis is nearly empty.** The malware-check tools that exist scan
  *installed systems* and pacman logs; none scans a built `.pkg.tar.zst`
  before install. The August 2026 `xsnow` campaign is the whole argument: the
  uploaded PKGBUILD was **clean**, and the payload rode inside the built
  package's `.INSTALL` — it never touched `source=()`, so no repository diff
  and no recipe scan could ever see it. AURCache *produces* the artifact, so it
  can inspect that. This design builds a small in-house rule engine for it and
  deliberately does not pretend a vendor exists.
- **LLM verdicts do not fit an unattended server.** The interactive tools
  (`aurscan` verdict mode, `Pacinspect`) are fail-closed and prompt a human at a
  terminal; AURCache's build loop has nobody to ask. Static rule output is the
  only shape that projects across the fleet, and it is what this design records.

## Motivation

Two earlier decisions leave a build with strong *containment* and no *opinion*:

- The worker runs `makepkg` (host-side fetch and chroot build) under
  `aurcache-sandbox`, and the server parses PKGBUILDs through the same binary
  (`backend/aurcache-utils/src/pkgbuild.rs`). Attacks are *stopped*; nothing is
  *characterized*.
- A build's "interesting" properties are not recorded anywhere structured.
  There is no field that says "this package ships an install scriptlet that
  curls a host it never declared", so there is nothing to review, filter, or
  one day gate on.

The gap between "an AUR package" and "a package this server built" is exactly
where a hostile actor hides. Recipe scanning catches the PKGBUILD that *looks*
hostile; artifact scanning catches the PKGBUILD that looks fine and produces
something hostile. A server that produces the final artifact and already holds
the built package for its clients can do the second one, and this design does.

## Changes

### 1. The signal shape (one JSON blob on the build row)

A new column `signals` on `builds` (`backend/aurcache-db/src/builds.rs:10`),
`TEXT` holding JSON, following the same encoded-string discipline as
`source_data` and `split_packages`. Written twice during a build's life, both
server-side and serialized by the build-state machine, so there is no race:

```json
{
  "recipe": {
    "scanner": "traur",
    "risk": "medium",
    "findings": [
      { "rule": "R-NET-FILE", "severity": "warn",
        "message": "network fetch inside package()", "file": "PKGBUILD" }
    ]
  },
  "artifacts": {
    "unreal-engine-5.8.2-1-x86_64.pkg.tar.zst": {
      "findings": [
        { "rule": "A-INSTALL-NET", "severity": "blocking?", "weight": 20,
          "message": ".INSTALL runs curl toward an undeclared host", "file": ".INSTALL" }
      ],
      "score": 20
    }
  },
  "score": 24
}
```

The artifact half is **keyed by file name**, because a build of a split package
stages several (`packages.split_packages` records them, and `read_staging`
returns whatever the build produced). Each is scanned on its own: they have
their own `.INSTALL`, their own units, their own file lists, and a payload that
rides in one subpackage is invisible in its siblings.

Each finding carries the weight it contributes. An artifact's `score` is the sum
of its own findings plus the recipe's — the recipe is shared by every subpackage
of the build, so it counts once in each — and the build's `score` is the
**highest** of those, not their sum: three subpackages shipping the same
scriptlet from one PKGBUILD are one suspicious recipe, not three, and summing
would make a package's score grow with how finely it splits.

A `severity` of `"blocking?"` is a *marker*, not a verdict:
nothing acts on it unless an operator sets a withhold threshold (Change 6),
which ships unset. Rule hits are capped (e.g. 50) and the message truncated per hit, so the
blob stays a bounded, log-line-sized object.

The typed shape lives in `aurcache-common` (`api`), added once and shared with
the CLI, the client library and the frontend, per the "shared, never
re-declared" convention. The `builds` entity keeps the raw string.

### 2. Recipe scan: pre-build on the worker, report to the build log

Runs immediately before the build, over the exact extracted tree — not the
snapshot from add time — so the signal reflects what this build actually ran
(package source moves between add and build; the tree does not).

- **Chroot worker**: in `run_job_inner`
  (`backend/aurcache-worker/src/job.rs:70`), right after
  `extract_source`/`make_source_writable` (`job.rs:113`, `job.rs:122`) and
  before staging configs/chroot (`job.rs:154`).
- **Docker worker**: in `DockerExecutor::build`, after `extract_source`
  (`backend/aurcache-worker-docker/src/executor.rs:140`).
- Both executors share the source-extraction code already
  (`backend/aurcache-worker-core/src/artifacts.rs:12`), so the scan wrapper is a
  shared helper in `aurcache-worker-core`, invoked by both workers — no logic
  per executor.

The scanner is a **subprocess**, named by `AURCACHE_RECIPE_SCANNER` (default
`/usr/bin/traur`; `ks-aur-scanner` is the documented alternate because it emits
JSON/SARIF directly, which makes the wrapper thinner), and it is run **through
`aurcache-sandbox`** like everything else that touches an untrusted tree: it is
third-party code reading attacker-supplied input, and a scanner with a parsing
bug is exactly the shape of thing that should not be the one unconfined reader
on the worker. It needs no network and no writes outside its own temp dir, so
`--no-net --isolate-ipc` and a read grant over `pkgdir` fit it exactly. The
worker:

1. runs the scanner over `pkgdir`,
2. writes its human-readable findings into the build log via the existing
   `log()` protocol call (imported in `job.rs:16`) as a
   clearly delimited `[worker] scan: …` block, so the report lands in the log *at
   scan time* while the build is still running, and
3. keeps the structured summary to ship with the completion report.

A scanner that is missing, times out, or exits nonzero is **not a build
failure**: the worker logs "scan unavailable" and builds on. The scan is
observation.

The recipe blob rides home in `CompleteReport` (`worker.rs:199-216`) as a new
`#[serde(default)] signals` field — old workers send none, and the server treats
that as an empty signal. This is the same seam the peak-memory figure already
crosses (`CompleteReport.peak_memory_bytes`, `worker.rs:215` →
`record_peak_memory`, `worker_complete.rs:56`), and it survives **failed** builds
too, which the publish-time artifact scan cannot: a package whose build failed
for unrelated reasons still carries its recipe suspicion. The server stashes the
recipe part onto the row in `accept_for_publishing`
(`backend/aurcache-utils/src/worker_complete.rs:143`), which is where the
successful completion is accepted.

### 3. Artifact scan: server-side at publish

The artifact scan runs in `publish` (`backend/aurcache-utils/src/publish.rs:
117`), against the staged `.pkg.tar.*` files (`read_staging`, `publish.rs:168`).
It reuses the machinery `describe_package` already has
(`backend/pacman-repo-utils/src/describe.rs:38-69`): a `zstd`/`xz` decoder over
a `BufReader` feeding `tar::Archive`.

**It streams, and the size rules are part of the design rather than a later
optimization.** This server publishes a 48 GiB `unreal-engine` package that
expands to 127 GiB across some 460,000 entries; decompressing that into memory
is not an option, and reading the archive a second time would add ten minutes to
every publish of it. So:

- entry *names* and modes come from the tar headers as they stream past, which
  is what most rules need and costs nothing beyond the pass;
- entry *contents* are read only for the metafiles the rules name (`.INSTALL`,
  `*.hook`, unit and `tmpfiles.d` files), each capped (say 256 KiB) and
  truncated past it;
- the scan shares `describe_package`'s single pass from the start. A helper
  `iter_package_entries` in `pacman-repo-utils` yields each entry's name, mode
  and a bounded reader, and both the describe step and the rules consume it.
  The *rules* stay in `aurcache-utils`, because a general repository library
  does not own security policy.

Initial artifact rules (the catalogue is deliberately tiny and specific — these
are the shapes the campaigns post artifacts in, not an open-ended "read
everything"):

| rule | signal |
|---|---|
| `A-INSTALL` | ships a non-trivial `.INSTALL` scriptlet at all |
| `A-INSTALL-NET` | `.INSTALL` references `curl`/`wget`/`fetch` toward a host not in the package's declared `source=` hosts |
| `A-HOOK` | ships `/usr/share/libalpm/hooks/*.hook` |
| `A-UNIT` | ships systemd units (`/usr/lib/systemd/system` or `/user`) |
| `A-UNIT-RESTART` | a shipped unit carries `Restart=always` / `RestartSec` / `OnBootSec` |
| `A-TMPFILES` | ships `/usr/lib/tmpfiles.d/*.conf` |
| `A-SETUID` | any shipped file has setuid/setgid bits |
| `A-ODD-EXEC` | executable files outside the standard binary/library dirs — informational only, and the noisiest rule here: anything installing a bundled application under `/opt` trips it in the thousands (`unreal-engine` alone), so it is capped early and never marked `blocking?` |

The findings are appended to the build log ("Published with N artifact signals —
see the build page") and the `artifact` half is written into the build row
inside the **same transaction** that records the publish (`record`,
`publish.rs:279`), merged with the recipe half already there. A scan failure is
logged and the publish proceeds with an empty `artifact` — the repository lock
must never be held by, or held up by, a heuristic.

### 4. Persisted signal plumbing

- **Migration**, mirroring `m20260907_000000_build_peak_memory.rs`: `ALTER TABLE
  builds ADD COLUMN signals TEXT;` for sqlite and postgres, down migration drops
  it.
- **Entity** (`backend/aurcache-db/src/builds.rs`): `signals: Option<String>`,
  written at `accept_for_publishing` (recipe part) and at `record()` (artifact
  part).
- **API** (`backend/aurcache-common/src/api/builds.rs:7`): `BuildSummary` gains
  `signals: Option<BuildSignals>` (parsed, with `#[serde(default)]` so old rows
  deserialize as `None`). One field added here reaches the CLI, the client and
  the frontend together.

### 5. UI

- **Build page** (`frontend-rs/src/screens/build.rs`): the existing header row
  gains a warning chip when the build's signals are non-empty, and an
  expandable "N suspicion signals" panel lists rule, severity and message per
  finding, grouped by artifact for a split build. A withheld build says so
  plainly — "held back from the repository: score 34 (threshold 30), from
  <name>; clients still have <previous version>" — with the per-artifact
  downloads and the Release action beside it. The poll loop already re-reads the build each cycle
  (`build.rs:398-417`), so the chip appears as soon as the completion report
  lands, and the artifact findings when the publish transaction commits.
- **Build rows** (`frontend-rs/src/screens/package.rs`, the `BuildRow` at
  `package.rs:432`, and `package_builds.rs:110-180`): a small warning marker on
  rows whose `signals` is non-empty. No new columns: the whole point of a
  signal is the *build you go look at*.

### 6. A score, and withholding a package from the repository

Rule hits are the evidence; a **score** is what a threshold can be set against.
Each rule carries a weight, the score is their sum for the build (recipe and
artifact together), and the blob records both the number and the rules that
produced it, so a score is always traceable back to what it came from. Weights
live beside the catalogue and change with it; a score is therefore comparable
between builds of the same AURCache version and not a number to store meaning
in beyond that.

The interesting action is not failing the build. A build that produced an
artifact has already spent its hours, and the artifact is the evidence — the
`xsnow`-shaped payload only exists *in* it. So the gate is about **publication**:

> `signals_withhold_score` (`Package -> Env -> Global -> Default`, default
> unset = never withhold). When a finished build's score is at or above it, its
> artifacts are kept and recorded but **not added to the repository**: no
> `repo.db` entries, so no client installs them, while an operator can still
> fetch them, unpack them, and decide.

**Withholding is per build, not per artifact**, even though scanning is per
artifact. One recipe produced the whole set, so a subpackage that scores is
evidence about the recipe, and publishing its siblings would ship code from a
PKGBUILD an operator has not cleared. Partial publication would also be a
worse artifact than either outcome: a split set whose members reference each
other, half in the repository and half not, is a dependency graph nobody asked
for. The build's score is the highest artifact score, and the whole set goes or
stays.

How it fits the publish path (`backend/aurcache-utils/src/publish.rs`):

- **A new terminal state, `WITHHELD`**, beside `SUCCESSFUL` and `FAILED_BUILD`
  in `BuildStates`. `PUBLISHING` already exists as the state between a worker's
  completion and the repository write, which is exactly where the decision
  belongs: `publish_build` scans the staged artifact (Change 3), computes the
  score, and either commits through `Repository` as today or moves the files to
  a quarantine directory beside the repository and ends the build `WITHHELD`.
- **No `files` rows, no `repo.db` entry.** An orphaned `files` row is not a
  harmless leak (publishing reads it as "already produced by another package"),
  so a withheld build records its artifact in its own table
  (`withheld_artifacts`: build, filename, size, sha256, path) rather than in
  `files`. The repository transaction is not entered at all, so the repository
  is untouched and its lock is never taken for a package nobody will install.
- **Where the file goes.** A quarantine directory beside the staging area under
  the repository root (`.quarantine`, as staging is `.staging`,
  `repository.rs:25`), so moving a staged artifact into it is a rename on one
  filesystem, as publishing already is. It is not in `repo.db`, so no client
  discovers it — but "not listed" is not "not served", and the mirror serves
  the repository root as files. The quarantine directory must therefore be
  excluded from what that server exposes, or live outside the root entirely;
  otherwise withholding only costs an attacker a guessed URL.
- **The previous version stays installable.** Retiring the file a new version
  replaces happens inside the repository commit, which a withheld build never
  reaches, so the last published version keeps its `repo.db` entry and its file.
  `Repository::sweep` judges by `repo.db` and still lists it, so it is not swept
  either. Clients simply keep installing the version they had — which is the
  behaviour you want from a quarantine, and falls out of not touching the
  repository rather than needing code.
- **Dependents do not fan out.** Rebuild fan-out and "is this version
  satisfied" both read *successful* builds; `WITHHELD` is not one, so a package
  depending on a withheld one keeps waiting rather than building against
  something the repository does not have.
- **The version is still recorded as built.** A withheld build must not make the
  scheduler rebuild the same version on its next pass — that would quarantine it
  again every interval, forever. The version check treats `WITHHELD` like
  `SUCCESSFUL` for "have we built this version", and like `FAILED_BUILD` for
  "may clients have it".

**Getting at the artifact.** A withheld package is evidence, and evidence that
cannot be opened is useless, so it is downloadable through the authenticated
API rather than the public repository:

- `GET /api/build/<id>/artifacts` lists what the build produced (name, size,
  `sha256`, and per-file findings), and `GET /api/build/<id>/artifact/<name>`
  streams one, with its `sha256` in the response headers, behind the same
  authentication every other API route uses. A split build therefore lists
  several and each is fetched by name.
- The build page lists them beside the signal panel, each with its own score and
  a download action, and `aurcache-cli builds artifacts <pkg>/<n>` /
  `builds artifact <pkg>/<n> <name> -o <path>` do the same from a shell, which
  is what an operator will actually use. The route is deliberately *not* the
  mirror: analysing a suspicious package is an operator action, not a public
  one.
- The same routes serve a published build's artifacts too, which costs nothing
  extra and saves knowing the repository URL layout.

Operator actions, all audited like other package changes:

- **Release** — publish the withheld artifact unchanged, through the normal
  `Repository` commit, moving the build to `SUCCESSFUL`. This is the reviewed
  path: the signal did its job, a person looked, the package is fine.
- **Discard** — delete the artifact and leave the build `WITHHELD`, or delete
  the build outright through the existing deletion path.

Not implemented in the first pass; the first pass is the signal and the score.
But the shape is worth fixing now, because it is what the score is *for*: a
number nobody can act on is a number nobody reads. The default stays unset —
flagging is what ships, withholding is an operator's choice, and a fleet that
has never looked at a report should not start having packages disappear from
its repository.

## What we deliberately do not do

- **Fail builds on signals.** Even with a threshold set, a scored build still
  finishes and keeps its artifact; what a threshold withholds is *publication*,
  because the artifact is the evidence and throwing it away is the one
  irreversible move available. Failing the build would also lose the hours it
  cost to find out.
- **Gate publication by default.** Legitimate packages ship
  `.INSTALL` scriptlets, hooks, and `Restart=always` units (`dockerd`, `containerd`,
  any watchdog). A heuristic that turned those into failures would break the
  fleet; the catalogue is honest and the gate is an explicit per-package opt-in.
- **LLM verdicts (fail-closed, human-in-the-loop).** The `aurscan`/`Pacinspect`
  style reviewers prompt and block; a server has no interactive session. If a
  deployment ever wants that depth, the signal blob is the feed they'd run it
  over — nothing here stops it.
- **An exec-based recipe linter on the worker.** `archlinux-inputs-fsck`
  *sources* the PKGBUILD when it lints, which is exactly what the sandboxed
  parse already does (`backend/aurcache-utils/src/pkgbuild.rs`) and what the
  worker's host-side fetch does under Landlock. Re-running it as a separate
  "lint" adds a second unsandboxed source-exec point; static-only scanners
  (traur, ks-aur-scanner) avoid that by design.
- **namcap as the security scan.** It is the established artifact-quality
  baseline, but its mandate is hygiene (permissions, dependency tagging), needs
  a chroot, and is not malware-shaped. Nothing stops an operator running it
  themselves; AURCache does not gate security on it.
- **Discarding the signal when the scanner is absent.** A worker image without
  the scanner binary still builds; the row records no recipe signal rather than
  failing the job. The signal is value-add, not a precondition.

## Consequences

- The `xsnow`-shaped attack (clean PKGBUILD, payload in the built `.INSTALL`)
  is now **visible at publish**: the artifact scan renders the scriptlet and the
  unit files that ships, and the build page says so to anyone who looks, even
  though nothing was ever blocked.
- Recipe suspicion survives **failed** builds (carried in `CompleteReport`), so
  a package whose build OOM'd still has its analysis attached.
- Build-time network keeps working. None of this touches what a build may
  connect to; it only records what the *recipe* and the *artifact* contain.
- Both scans are report-only pass-by. The recipe scan adds a bounded static
  pass pre-build; the artifact scan adds one archives-streaming read at publish,
  which can later be fused into the `describe_package` pass
  (`describe.rs:38-69`) that already reads the archive once.
- Old workers keep working: `CompleteReport.signals` defaults empty, and the
  OC field deserializes old rows untouched.

## Verification

- **Unit (rule engine).** The artifact rules run on synthetic packages built
  in-test exactly as the `discover_artifacts` tests build their tarballs
  (`backend/aurcache-worker-core/src/artifacts.rs:68-85`): a package with a
  hostile `.INSTALL` yields `A-INSTALL-NET`; a clean `hello`-shaped package
  yields zero findings; each rule's file-name trigger is asserted directly.
  `builds.signals` round-trips as `None` for `NULL` and parses the two-part JSON.
- **Unit (worker wrapper).** The shared recipe-scan helper runs against a stub
  scanner script (`AURCACHE_RECIPE_SCANNER` pointing at an executable emitting a
  fixed JSON fixture), asserting: findings captured, missing binary logged and
  non-fatal, non-zero exit logged and non-fatal, bounded hit list.
- **Integration.** `cargo test -p aurcache-utils --test publish` drives
  `publish_build` against a staged hostile artifact and asserts the build row's
  `signals.artifact` is populated and publish **succeeded** — the flip side of
  the existing `publish.rs` tests, proving signals never gate.
- **Frontend.** Render tests for the build page and build rows: a `signals`
  payload renders the chip and the panel; an empty one renders neither
  (Dioxus host-render tests, the same shape as the existing `build.rs` tests).
- **E2E.** `scripts/test-e2e.sh` with the worker image shipping the scanner: a
  real package build completes, publishes, and its page shows the signal —
  while a `hello`-style fixture builds clean with no banner.

Full gate: `just lint`, `just test`, `just test-browser`.