---
sidebar_position: 2
---

# Quick Start

AURCache runs as two pieces: the **server**, which holds the package database
and serves the web UI and the pacman repository, and one or more **build
workers**, which build packages and upload the results. The bundled compose file
starts one of each on a single host.

```bash
curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/docker-compose.yaml
docker compose up -d
```

- Web UI / API: `http://localhost:8080` (plain HTTP; front it with a reverse proxy for TLS)
- Pacman repo: `http://localhost:8081` (add as a `[repo] Server` in `pacman.conf`)

The worker enrolls itself and starts polling for jobs within a few seconds — no
token and no approval click. Add build capacity with `docker compose up -d
--scale builder=3`, or raise `WORKER_CONCURRENCY` on the worker.

## With PostgreSQL

SQLite is fine for trying AURCache out, but PostgreSQL is recommended for
anything you intend to keep:

```yaml
services:
  aurcache:
    image: ghcr.io/lukas-heiligenbrunner/aurcache-server:latest
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
    image: ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest
    depends_on:
      - aurcache
    environment:
      - AURCACHE_URL=https://aurcache:8083
      - AURCACHE_ENROLLMENT_DIR=/enroll
      - WORKER_CONCURRENCY=2
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
    image: postgres:latest
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

## Upgrading from a single-container setup

If you already run AURCache as one container, it keeps working: see
[backward compatibility](../setup/docker.md#backward-compatibility-the-hybrid-image).

For more advanced setup see the
[Configuration](/docs/Configuration/environment-variables) page.
