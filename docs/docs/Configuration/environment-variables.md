---
sidebar_position: 1
---

# Environment Variables
AURCache can be configured using the following environment variables:

## Settings precedence

Most settings can also be set in the UI — globally on the Settings page or
per-package on the package's Settings page. When the same setting is
configured in multiple places, the resolution order is, from highest to
lowest priority:

1. **Per-package setting** — explicit user intent for one package always
   wins. This lets you override an env-imposed baseline for a single
   package (e.g. give one heavy build more memory) without touching the
   deployment defaults.
2. **Environment variable** — the admin's deploy-time override of the
   global default. Locks the global Settings page tile, but does **not**
   prevent per-package overrides.
3. **Global setting** — UI-set baseline stored in the database.
4. **Static default** — built-in fallback (the "Default" column below).

The UI badges reflect the source: `(default)`, `(inherited)` (= global),
`(inherited from env)`, or no badge when this scope owns the value.

## Database Configuration
| Variable               | Type                  | Description                                                         | Default                   |
|------------------------|-----------------------|---------------------------------------------------------------------|---------------------------|
| DB_TYPE                | (POSTGRESQL\| SQLITE) | Type of Database (SQLite, PostgreSQL)                               | SQLITE                    |
| DB_USER                | String                | POSTGRES Username  (ignored if sqlite)                              | null                      |
| DB_PWD                 | String                | POSTGRES Password  (ignored if sqlite)                              | null                      |
| DB_HOST                | String                | POSTGRES Host   (ignored if sqlite)                                 | null                      |
| DB_NAME                | String                | Database name                                                       | 'db.sqlite' or 'postgres' |

## General Settings

| Variable               | Type          | Description                                                           | Default |
|------------------------|---------------|-----------------------------------------------------------------------|---------|
| VERSION_CHECK_INTERVAL | Integer       | Interval in seconds for checking package versions                     | 3600    |
| AUTO_UPDATE_SCHEDULE   | String (CRON) | Auto update schedule in cronjob syntax with seconds (null to disable) | null    |
| LOG_LEVEL              | String        | Log level                                                             | INFO    |
| JOB_TIMEOUT            | Integer       | Longest a build may run before the server reclaims it, in seconds     | 3600    |
| SECRET_KEY             | String        | \>32Byte Random String for singing cookies                            | Random  |
| AURCACHE_PUBLIC_URL    | String        | Base URL workers use for the pacman repo, baked into build configs. Example: `http://aurcache:8081` | `http://localhost:8081` |

Builds run on [build workers](../workers/configuration.md), which are configured
on the worker itself rather than here — including how many builds it runs at
once and how long it will let one run.

:::note Legacy build settings
`BUILDER_IMAGE`, `CPU_LIMIT`, `MEMORY_LIMIT` and `BUILD_ARTIFACT_DIR`
configured the per-build Docker container that AURCache used to spawn. In the
split setup they do nothing: resource limits are whatever the worker's own
container or host imposes, and the worker uploads packages over the API rather
than through a shared directory.

They are still read by the [hybrid compatibility
image](../setup/docker.md#backward-compatibility-the-hybrid-image), where they
keep their original meanings — `BUILD_ARTIFACT_DIR` is in fact what selects the
legacy builder there.

`MAX_CONCURRENT_BUILDS` is replaced by `WORKER_CONCURRENCY` on each worker, and
`AURCACHE_REPO_URL` by `AURCACHE_PUBLIC_URL`.
:::

## Build workers

Workers connect to a dedicated mTLS listener and are configured through their
own environment. See [Build Workers](../workers/configuration.md).

| Variable                | Type    | Description                                                  | Default |
|-------------------------|---------|--------------------------------------------------------------|---------|
| AURCACHE_WORKER_PORT    | Integer | Port for the worker protocol listener                        | 8083    |
| AURCACHE_TLS_SANS       | String  | Hostnames the worker listener's certificate is valid for     | localhost |
| WORKER_SPILL_DELAY      | Integer | Seconds before worker priority stops holding a job back      | 60      |
| WORKER_LIVENESS_TIMEOUT | Integer | Seconds before a quiet worker stops counting as available    | 60      |
| MAX_ATTEMPTS            | Integer | Requeues before a build is failed for good                   | 3       |
