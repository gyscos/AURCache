---
sidebar_position: 6
---

# Native install (systemd)

AURCache runs on the host without Docker, as two systemd services. This is
often the better choice for the worker: several things the container images do
exist only *because* they are containers, and drop away here.

- `systemd-nspawn` needs a systemd manager, which a container lacks — the image
  ships a wrapper for it. A real host has one.
- pacman 7's Landlock download sandbox cannot initialise in a nested container,
  so the images disable it. Natively it works.
- No privileged container, and no qemu/binfmt for the machine's own
  architecture.

## Packages

```bash
pacman -S aurcache-server   # the build server and web UI
pacman -S aurcache-worker   # a build machine
```

They are separate because their dependencies are: the server needs no
`devtools`, `base-devel` or `sudo`, and most build machines do not want a
server. Both may be installed together on one host; they share the `aurcache`
user.

## Server

```bash
$EDITOR /etc/aurcache/server.env
systemctl enable --now aurcache
```

It listens on **8080** (API and web UI), **8081** (the pacman repository) and
**8083** (workers). State lives in `/var/lib/aurcache`: the repository, the
database, the worker CA and the build logs.

### Build logs are files, not database rows

Each build's log is `build_logs/<pkgbase>/<number>.log` under the server's
working directory, overridable with `AURCACHE_BUILD_LOG_PATH` — named after the
build's public identity, the same `<pkgbase>/<number>` the API, the CLI and the
web UI use. They were a `TEXT`
column until they got too expensive to be one: appending to a row means the
database rewrites the whole value, so a 31 MB log cost gigabytes of writes over
a build's lifetime, and reading a tail meant fetching the entire log to discard
most of it. A file appends and seeks in proportion to what actually changed.

Two consequences worth knowing:

- **A database backup no longer contains build logs.** They are derived data —
  the dump format has always excluded them for that reason — but if you were
  relying on `pg_dump` to capture them, include the log directory instead.
- **A log whose file is missing is not an error.** The build page shows "no log
  for this build" rather than failing, which is also what you get for a build
  that never produced output.

Upgrading moves existing logs out of the database automatically and then drops
the column. That migration is one-way: take a backup first if the logs matter
to you.

### The PKGBUILD parser is sandboxed

Reading a PKGBUILD means *sourcing* it, so every package the server inspects
runs bash in the process that holds the database credentials and owns the
repository. The server never runs `alpm-pkgbuild-bridge` directly: it runs it
through `aurcache-sandbox`, naming both by absolute path, and refuses to parse
at all if that binary is missing. Nothing depends on `PATH`, so nothing about
your environment can quietly unconfine a parse.

Each parse may write only the PKGBUILD's own directory, and cannot read the
server's working directory (the database, the repository and the CA) or
`/etc/aurcache` (where `server.env` keeps the database password and the OAuth
secret). Its environment is replaced, so a PKGBUILD cannot read a secret out of
it either. It also cannot signal the processes around it, which needs Linux
6.12 — the server's minimum.

TCP is denied as well, but that is hardening rather than a boundary: Landlock
only covers TCP, so UDP and DNS still leave the machine. It is there because a
parse has no reason to connect anywhere, not because it contains one that
tries. If a package could be added that you would mind phoning home, a host
firewall is the control that stops it.

### When a package needs the network to parse

About one AUR package in five hundred computes its version while being sourced,
with `pkgver=$(curl -s https://api.github.com/…)` or `git ls-remote`. Denied the
connection, `pkgver` comes out empty and the parse fails on the missing version
rather than on the connection — so the error says which command wanted the
network and names the setting below.

`parse_network` (or `PARSE_NETWORK=true`) lifts the TCP denial for parsing.
Everything else stays: the parse still writes only its own directory, still
cannot read the server's state, still gets no secrets in its environment, and
is still isolated from the processes around it. It is a global setting today,
so turn it on only if you would run those packages' `pkgver` command yourself.

`AURCACHE_PROTECTED_DIR` names further directories to keep unreadable
(colon-separated). It adds to the list above rather than replacing it.

Everything is optional in `server.env`; the defaults work for a single machine.
Two worth setting before anything else reaches it:

- `AURCACHE_URL` — how workers and pacman clients address this server.
- The `OAUTH_*` block. With all of them unset there is **no authentication**,
  which is only sensible on a machine nothing else can reach. See
  [Authentication](../Configuration/authentication.md), and note
  `OAUTH_ALLOWED_USERS` if your provider is a public one.

## Worker

```bash
$EDITOR /etc/aurcache/worker.env    # AURCACHE_URL and an enrolment token
systemctl enable --now aurcache-worker
```

Get the enrolment token from the server's **Workers** page. After the first
successful enrolment the worker authenticates with its own certificate and the
token is no longer used.

### What the package sets up

Two users, and the split between them is the security model rather than
tidiness:

| | |
|---|---|
| `aurcache` | runs the worker process; owns the mTLS identity and build credentials, and is the only user that can read them |
| `builder` | runs each package build, named explicitly by `makechrootpkg -U`, so a build never inherits the worker's user and cannot reach its secrets by file permissions |

`builder`'s **primary** group is `aurbuild`. That is load-bearing, not
incidental: `makechrootpkg` carries only the build user's primary uid/gid into
the chroot, so a supplementary group does not exist in there and group-write on
the caches fails from *inside* the chroot, after every host-side check has
already passed.

Directories, with ownership that matters for the same reason:

```
/var/lib/aurcache-worker           aurcache  worker state
/var/lib/aurcache-worker/secrets   aurcache  0700, build credentials
/var/lib/aurcache-worker/work      builder   2775, shared to the worker by group
/var/lib/aurcache-worker/chroot    aurcache  the storage pool: its image and mount point
```

Everything that grows -- the base chroot, each build, the source and package
caches -- lives in the storage pool, a btrfs filesystem the worker creates in
`chroot/pool.img` and mounts at `chroot/pool`, with a disk quota per build and
one over the whole worker. See the worker configuration's section on disk
quota.

### Why the worker's unit is not hardened

`aurcache.service` takes the full set of systemd hardening directives.
`aurcache-worker.service` deliberately does not, and the unit says so inline.
Its whole job is `sudo mkarchroot` and `sudo makechrootpkg`, which mount,
unshare and chroot. `NoNewPrivileges=yes` alone stops every build before it
starts; `ProtectSystem=strict` makes the chroot and pacman's cache read-only;
`RestrictNamespaces=yes` forbids what `systemd-nspawn` *is*.

The isolation that matters here is not systemd's. Each build runs in its own
devtools chroot as a different unprivileged user, and the two places a PKGBUILD
is executed *outside* that chroot are confined with Landlock.

### The patched `makechrootpkg`

`makechrootpkg` executes the PKGBUILD twice outside the chroot — once for
`makepkg --verifysource`, once to read `pkgbase`/`pkgname`. A PKGBUILD is bash,
so unconfined either one lets a build write another build's files. Checksums are
no defence: they are declared by the PKGBUILD being run.

The package installs a copy at `/usr/lib/aurcache/bin/makechrootpkg` with those
two sites wrapped in `aurcache-sandbox`, and the worker names that path
absolutely. It is derived at install time from the `devtools` on **this**
machine rather than shipped pre-patched, because `makechrootpkg` sources six
files from `/usr/share/devtools/lib` and `/usr/share/makepkg/util` at runtime —
a copy taken against one devtools release would end up running against
another's libraries.

A pacman hook re-derives it whenever `devtools` is installed or upgraded. If a
future devtools moves one of those two call sites the hook **fails the
transaction and removes the stale copy**, so the worker refuses to build rather
than quietly building with two steps unconfined. If that happens, please report
it with your `devtools` version.

Nothing is installed into `/usr/local`, and nothing shadows `makechrootpkg` for
other users: running it yourself gets the packaged one.

## Both on one machine

Install both packages. They share the `aurcache` user and their state
directories are separate, so no extra configuration is needed — point
`AURCACHE_URL` in `worker.env` at `http://localhost:8080`.

## Building the packages yourself

```bash
cd packaging/aurcache-server && makepkg -si
cd ../aurcache-worker      && makepkg -si
```

Two PKGBUILDs rather than one split package: the roles share almost no
dependencies, and `makepkg` cannot build one half of a split — which the
container images need, since a worker image has no reason to carry the server.

`makepkg -si` installs the build dependencies itself, with one exception: the
server's `wasm-bindgen-cli` is an AUR package, so install it with an AUR helper
first. Its version has to match the `wasm-bindgen` crate in
`frontend-rs/Cargo.lock` — wasm-bindgen refuses a mismatched pair rather than
producing a subtly broken bundle. The worker has no such dependency; it does
not carry the web UI.

Both PKGBUILDs set `options=(!lto)`. makepkg's LTO adds `-flto=auto` to
`CFLAGS`, and the `cc` crate hands that to the vendored C in `aws-lc-sys` and
`ring`; their static archives then hold GCC LTO bytecode, which `ld.lld` — the
linker rustc drives — cannot read. The build fails at link with undefined
`aws_lc_*` symbols and no error from the build script, which is an unpleasant
thing to diagnose from scratch. Rust's own LTO is cargo's business and is
unaffected.

### Cross-compiling

Arch has no cross-compilation mode — no `--target`, and nothing in `devtools` —
but `CARCH` is a plain shell variable `makepkg` uses to label the package, so
exporting it and cross-compiling inside `build()` produces a correctly labelled
result:

```bash
pacman -S aarch64-linux-gnu-gcc
cd packaging/aurcache-worker
CARCH=aarch64 makepkg --nodeps --nocheck
```

`--nodeps` because dependency resolution would consult the *host's*
repositories, and `--nocheck` because the tests cannot run binaries built for
another architecture. This is what the container images do.

Only `x86_64` and `aarch64` are supported. The aarch64 toolchain is in `extra`;
armv7's is not packaged officially, and Arch Linux ARM ships no x86_64 cross
toolchain at all — so the images must be built on an x86_64 host.
