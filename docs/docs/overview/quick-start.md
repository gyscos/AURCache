---
sidebar_position: 2
---

# Quick Start

AURCache runs as two pieces: the **server**, which holds the package database
and serves the web UI and the pacman repository, and one or more **build
workers**, which build packages and upload the results. The bundled compose file
starts one of each on a single host.

```bash
curl -O https://raw.githubusercontent.com/gyscos/AURCache/main/docker-compose.yaml
docker compose up -d
```

- Web UI / API: `http://localhost:8080` (plain HTTP; front it with a reverse proxy for TLS)
- Pacman repo: `http://localhost:8081` (add as a `[repo] Server` in `pacman.conf`)

The worker enrolls itself and starts polling for jobs within a few seconds — no
token and no approval click. Add build capacity with `docker compose up -d
--scale builder=3`, or raise the worker's concurrency on its page in the web
UI.

## With PostgreSQL

SQLite is fine for trying AURCache out, but PostgreSQL is recommended for
anything you intend to keep:

```yaml
services:
  aurcache:
    image: ghcr.io/gyscos/aurcache-server:latest
    ports:
      - "8080:8080"   # Web UI + API
      - "8081:8081"   # Pacman repository
      - "8083:8083"   # Worker protocol (HTTPS + mutual TLS)
    volumes:
      - ./aurcache/repo:/app/repo
      - aurcache_ca:/app/data/ca   # internal worker CA; persist across restarts
      - enroll:/enroll:ro
    environment:
      - AURCACHE_TLS_SANS=aurcache,localhost
      - AURCACHE_PUBLIC_URL=http://localhost:8081
      - AURCACHE_ENROLLMENT_DIR=/enroll
      - DB_TYPE=POSTGRESQL
      - DB_USER=aurcache
      - DB_PWD=YOUR_SECURE_PWD
      - DB_HOST=dbhost
    networks:
      aurcache_network:
    restart: unless-stopped

  builder:
    image: ghcr.io/gyscos/aurcache-worker:latest
    depends_on:
      - aurcache
    environment:
      - AURCACHE_URL=https://aurcache:8083
      - AURCACHE_ENROLLMENT_DIR=/enroll
      - WORKER_CONCURRENCY_DEFAULT=2
    volumes:
      - enroll:/enroll
      - worker_data:/var/lib/aurcache-worker
      - worker_cache:/var/cache/aurcache-worker
    # Each package is built in a systemd-nspawn chroot, which needs mounts and
    # namespaces a plain container forbids.
    privileged: true
    tmpfs:
      - /run
    networks:
      aurcache_network:
    restart: unless-stopped

  aurcache_database:
    # Pin both the major version and the Debian release. A new major version
    # will not start on the old one's data directory, and a new Debian release
    # changes how text sorts under indexes already built.
    image: postgres:17-trixie
    volumes:
      - ./aurcache/db:/var/lib/postgresql/data
    environment:
      - POSTGRES_PASSWORD=YOUR_SECURE_PWD
      - POSTGRES_USER=aurcache
    restart: unless-stopped
    networks:
      aurcache_network:
        aliases:
          - "dbhost"

volumes:
  aurcache_ca:
  enroll:
  worker_data:
  worker_cache:

networks:
  aurcache_network:
    driver: bridge
```

The server and the worker authenticate to each other with mutual TLS on port
8083. Sharing the `enroll` volume is what tells the server this worker is
trusted, so no secret has to be configured. For a worker on another machine —
where there is no shared volume — see
[Build Workers](../workers/configuration.md).

Keep the Postgres image pinned as shown rather than `postgres:latest` or even
`postgres:17`. Those tags move to a newer Debian release from time to time, and
a database whose indexes were sorted by the old C library then needs its
indexes rebuilt. AURCache checks for that at startup and logs the commands to
run if it finds one (`REINDEX DATABASE` then `ALTER DATABASE ... REFRESH
COLLATION VERSION`, in each database the warning names). Moving to a new major
version of Postgres is a separate upgrade, with `pg_upgrade` or a dump and
restore.

## Or let the CLI do it

If you have `aurcache-cli` (`cargo install aurcache-cli`), it can stand the whole
thing up. Nothing here needs a token or a running server — it is the command for
when you have neither:

```bash
aurcache-cli setup server      # the backend, on this machine
aurcache-cli setup worker      # one build worker beside it
aurcache-cli doctor            # check they found each other
```

The worker approves itself. `setup` gives the pair a shared `enroll` volume, the
same trick the compose file above uses, so there is no approval step and no
secret to configure.

Add `--dry-run` to either command to print the `docker run` line instead of
running it.

More workers on the same machine each need their own name and identity volume:

```bash
aurcache-cli setup worker --container-name worker-2
```

A worker on *other* hardware joins over the network, where trust has to be
explicit — pin the server's CA fingerprint from its startup log:

```bash
aurcache-cli setup worker \
  --server-url https://aurcache.example.com:8083 \
  --ca-fingerprint <sha256> \
  --arch aarch64
```

### For TrueNAS, Portainer, Unraid…

Anything that takes a compose file gets one:

```bash
aurcache-cli setup compose --role bundle    # server + a local worker
aurcache-cli setup compose --role backend   # server alone
aurcache-cli setup compose --role worker    # a worker for another machine
```

`-o -` writes to stdout to paste into a web UI; otherwise it writes
`docker-compose.yaml` (or `docker-compose.<role>.yaml`) and refuses to clobber an
existing file without `--force`. The output keeps the comments explaining why the
worker needs `privileged` and what the `enroll` volume is for, because those are
the parts worth reading before you deploy it.

A file with a server asks which database it should use, or takes
`--database postgres` or `--database sqlite` (required when there is no terminal
to ask on). PostgreSQL adds a database service pinned to `postgres:18-trixie`.
Its password is generated unless you pass `--db-password`, and since the database
publishes no port, it only has to match between the two services.

It also asks whether to add an upgrade step, or takes `--postgres-upgrade` or
`--no-postgres-upgrade`. With no terminal to ask on, the step is left out. The
step is `ixsystems/postgres-upgrade`, the one TrueNAS's own apps use, and runs
before the database and exits. It does nothing while the data already matches
the database's major version. After you raise `TARGET_VERSION` and the image tag
together, it backs the data up and runs `pg_upgrade`. It also moves data from the
older layout (mounted straight at `/var/lib/postgresql/data`) into the one the
file uses. Without the step, changing the image's major version starts an empty
database beside the old data, and upgrading is up to you.

## Filling it, and using it

A fresh instance is empty. If the machine you are on already installs packages
from the AUR, that is the set you probably want mirrored, and `pacman -Qm` is
what lists it:

```bash
aurcache-cli pkg add --from-installed
```

It prints what it found and asks before submitting — adding a package resolves
its dependencies and enqueues builds for them, so a long list is not something
to send unseen. Pass `--yes` to skip the prompt in a script, where it is
otherwise refused rather than assumed.

Once something has built, wire the repository into pacman:

```bash
aurcache-cli repo config | sudo tee -a /etc/pacman.conf
sudo pacman -Sy
```

If a build does not start, [`aurcache-cli doctor`](../workers/managing.md#why-is-a-build-not-starting)
says why.

`aurcache-cli completions <shell>` prints a completion script for bash, zsh,
fish, elvish or powershell.

## Upgrading from a single-container setup

If you already run AURCache as one container, it keeps working: see
[backward compatibility](../setup/docker.md#backward-compatibility-the-hybrid-image).

For more advanced setup see the
[Configuration](/docs/Configuration/environment-variables) page.
