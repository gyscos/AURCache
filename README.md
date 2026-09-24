<p align="center">
    <a href="https://gyscos.github.io/AURCache/">
        <img src="assets/icons/icon.svg" width="200"></a><!-- </a> being on the same line as the <img> tag is intentional! -->
    <br>
    <br>
    <a href="https://github.com/gyscos/aurcache/releases">
        <img src="https://img.shields.io/github/v/release/gyscos/aurcache?style=flat&labelColor=1C2C2E&color=C96329&logo=GitHub&logoColor=white"></a>
    <a href="https://github.com/gyscos/AURCache/pkgs/container/aurcache">
        <img src="https://ghcr-badge.egpl.dev/gyscos/aurcache/tags?color=%23c26632&ignore=latest&n=3&label=Tags&trim="></a>
    <a href="https://github.com/gyscos/AURCache/pkgs/container/aurcache">
        <img src="https://ghcr-badge.egpl.dev/gyscos/aurcache/size?color=%23c26632&tag=latest&label=Size&trim="></a>
    <br>
</p>

<h4 align="center">
  <a href="https://gyscos.github.io/AURCache/docs/overview/introduction">Documentation</a> |
  <a href="https://gyscos.github.io/AURCache/">Website</a>
</h4>

# AURCache

AURCache is a build server and repository for Archlinux packages sourced from the AUR (Arch User Repository). It features a Rust frontend and backend, enabling users to add packages for building and subsequently serves them as a pacman repository. Notably, AURCache automatically detects when a package is out of date and displays it within the frontend.

<p><img src="docs/static/img/screenshot1.png" alt=""/> 

## Quickstart

Bring up the server plus a local build worker with a single command — no edits,
no secrets, no approval clicks:

```bash
curl -O https://raw.githubusercontent.com/gyscos/AURCache/main/docker-compose.yaml
docker compose up -d
```

- Web UI / API: `http://localhost:8080` (plain HTTP; front with a reverse proxy for TLS)
- Pacman repo: `http://localhost:8081` (add as a `[repo] Server` in `pacman.conf`)

The bundled worker auto-enrolls via a shared volume and starts polling for jobs
within seconds. Scale local build throughput with `docker compose up -d --scale
builder=3` or raise the worker's concurrency on its page in the web UI. To attach a worker on separate
hardware or a foreign architecture (e.g. aarch64), see
[`docker-compose.remote-worker.yaml`](docker-compose.remote-worker.yaml).

Already running AURCache as a single container? It keeps working — the
`aurcache` image now bundles a build worker for exactly that case. It is
deprecated, so migrate to the split setup when convenient; see
[Backward compatibility](https://gyscos.github.io/AURCache/docs/setup/docker#backward-compatibility-the-hybrid-image).

## CLI client

A typed CLI for the HTTP API lives in `backend/aurcache-cli`. It authenticates with the API token via `Authorization: Bearer <token>`.
The reusable Rust client library backing it lives in `backend/aurcache-client`.

```bash
export AURCACHE_URL=http://localhost:8080/api
export AURCACHE_TOKEN=your-token

cargo run -p aurcache-cli -- pkg list
cargo run -p aurcache-cli -- pkg add paru --platform x86_64
cargo run -p aurcache-cli -- builds list --limit 10
cargo run -p aurcache-cli -- token regenerate
```

The CLI also reads `~/.config/aurcache-client/config.json`. Resolution order is:

1. `--url` / `--token`
2. `AURCACHE_URL` / `AURCACHE_TOKEN`
3. `~/.config/aurcache-client/config.json`
4. interactive prompt, with the prompted values saved back to the config file

Useful config commands:

```bash
cargo run -p aurcache-cli -- config show
cargo run -p aurcache-cli -- config set-url http://localhost:8080/api
cargo run -p aurcache-cli -- config set-token
```

Use `--format json` for machine-readable output, or `raw` for endpoints that do not have a dedicated subcommand yet.

<details>
<summary>More Images:</summary>
<br>
<img src="docs/static/img/screenshot2.png" alt=""/> 
<img src="docs/static/img/screenshot3.png" alt=""/></p>
</details>

## License

This project is licensed under the MIT License. Feel free to contribute and modify as per the guidelines outlined in the license agreement.
