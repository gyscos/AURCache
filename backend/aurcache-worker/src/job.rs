//! End-to-end execution of a single claimed job: download the (server-patched)
//! source, prepare a per-package chroot copy, import trusted keys, build under a
//! resource-limited scope while streaming logs and honoring cancellation, then
//! upload artifacts. Returns a terminal [`CompleteReport`]; never panics past
//! the caller's wrapper.

use anyhow::{Context, Result};
use aurcache_common::worker::{CompleteReport, JobDescriptor};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::{log, remote_cancel, upload_artifacts};
use aurcache_worker_core::{artifacts, report};

use crate::build;
use crate::cache::Cache;
use crate::chroot;
use crate::config::Config;
use crate::credentials;

/// Run one job to completion and return its terminal report.
///
/// The per-job workspace is removed here rather than at the end of the happy
/// path, so every early exit — source download, chroot prep, build failure,
/// artifact upload — drops its extracted sources and partial artifacts too.
/// Build ids are per-attempt, so a leaked workdir would never be reclaimed by a
/// later retry and `WORKER_DATA_DIR/work/` would grow without bound.
pub async fn run_job(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: JobDescriptor,
    cancel: Arc<AtomicBool>,
    active_pkgbases: Arc<Mutex<HashSet<String>>>,
) -> CompleteReport {
    let workdir = cfg
        .core
        .data_dir
        .join("work")
        .join(job.build_id.to_string());

    let report = match run_job_inner(cfg, client, &job, &cancel, &active_pkgbases, &workdir).await {
        Ok(report) => report,
        Err(e) => {
            let msg = format!("build setup failed: {e:#}");
            let _ = client
                .append_log(job.build_id, &format!("\n[worker] {msg}\n"))
                .await;
            report::setup_failure(msg)
        }
    };

    // Keep the shared caches and base chroot; only this job's tree goes.
    if let Err(e) = tokio::fs::remove_dir_all(&workdir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to clean workdir {}: {e}", workdir.display());
    }
    report
}

async fn run_job_inner(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    cancel: &AtomicBool,
    active_pkgbases: &Mutex<HashSet<String>>,
    workdir: &Path,
) -> Result<CompleteReport> {
    let build_id = job.build_id;
    let cache = Cache::new(
        &cfg.cache_dir,
        cfg.cache_max_size,
        cfg.cache_ttl,
        cfg.pkgcache_max_size,
        cfg.pkgcache_ttl,
    );

    // Opportunistic cache GC (never blocks the build). Pin every pkgbase that is
    // currently building — not just this job's — so a concurrent sibling's
    // in-progress SRCDEST is never wiped out from under it.
    //
    // The scan recurses the whole srcdest tree (up to `cache_max_size`) with
    // blocking `read_dir`/`metadata`, so it runs on the blocking pool: with
    // `concurrency` jobs starting at once it would otherwise tie up that many
    // runtime worker threads and stall heartbeats, claims, and log streaming.
    let in_use: Vec<String> = {
        let guard = active_pkgbases.lock().await;
        guard.iter().cloned().collect()
    };
    {
        let cache = cache.clone();
        let _ = tokio::task::spawn_blocking(move || cache.evict(&in_use)).await;
    }

    // 1. Fetch + extract source.
    log(client, build_id, "[worker] downloading source\n").await;
    let source = client
        .source(build_id)
        .await
        .context("downloading source")?;
    // Defensive: a crash mid-job could have left a tree behind under this id.
    let _ = std::fs::remove_dir_all(workdir);
    let pkgdir = artifacts::extract_source(&source, workdir).context("extracting source")?;

    // 2. Stage build credentials, then write per-package configs + ensure base
    //    chroot. The key is read now rather than at worker start, so replacing
    //    it on the host takes effect on the next build without a restart.
    let secrets_dir = credentials::secrets_dir(&cfg.core.data_dir);
    let credential =
        credentials::stage_for_job(cfg, &secrets_dir).context("staging build credentials")?;
    if credential.is_some() {
        log(
            client,
            build_id,
            "[worker] build ssh credential available\n",
        )
        .await;
    }

    let cfg_dir = workdir.join("config");
    // The server sends no `[repo]` section; the worker appends one rendered for
    // the host it reaches the server on (see aurcache_worker_core::repo).
    let pacman_conf =
        aurcache_worker_core::repo::append_to_pacman_conf(&job.pacman_conf, client.repo_section());
    let (makepkg_conf, pacman_conf) = chroot::write_configs(
        &cfg_dir,
        &credentials::augment_makepkg_conf(&job.makepkg_conf, credential.as_ref()),
        &pacman_conf,
        job.mirrorlist.as_deref(),
        cache.pacman_pkg().as_deref(),
    )
    .context("writing job configs")?;

    log(client, build_id, "[worker] preparing chroot\n").await;
    chroot::ensure_base_chroot(&cfg.chroot_dir, &pacman_conf, &makepkg_conf)
        .await
        .context("preparing base chroot")?;

    // 3. Import trusted PGP keys into the shared keyring.
    if let Some(gnupg) = cache.gnupg_home() {
        chroot::import_pgp_keys(&gnupg, &cfg.keyserver, &job.pgp_keys)
            .await
            .ok();
    }

    if cancel.load(Ordering::SeqCst) {
        return Ok(report::classify_exit_canceled());
    }

    // 4. Build under a resource-limited scope, honoring cancel.
    log(client, build_id, "[worker] starting build\n").await;
    // Private writable pacman cache for this job. Concurrent builds otherwise
    // share one writable cache directory and race on the same partial
    // download; the shared cache remains available read-only for hits.
    let job_label = format!("job-{build_id}");
    let pkg_cache_bind = cache
        .pacman_pkg_job(&job_label)
        .map(|dir| (dir, PathBuf::from(chroot::PER_JOB_CACHE_MOUNT)));

    // Operator-configured mounts, then this job's private pacman cache. Ours
    // land last on the systemd-nspawn command line, which is what lets the
    // per-job cache override arch-nspawn's own bind of the shared one.
    //
    // The build credential is deliberately *not* here. `makepkg` fetches
    // sources on the worker before the chroot is entered, so the key is never
    // needed inside it — and binding it in would hand it to whatever the
    // PKGBUILD chooses to run there.
    let mut binds = cfg.bind_mounts.clone();
    binds.extend(pkg_cache_bind);

    let report = run_build(cfg, client, job, &pkgdir, &cache, &binds, cancel).await?;

    // 5. Fold this job's downloads into the shared cache, then bound it. Only
    //    after `makechrootpkg` has exited: promoting mid-build would move
    //    packages out from under the running pacman.
    {
        let cache = cache.clone();
        let label = job_label.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let promoted = cache.promote_job_pkgs(&label);
            cache.wipe_pacman_pkg_job(&label);
            let evicted = cache.evict_pkgs();
            if promoted > 0 || !evicted.is_empty() {
                tracing::info!(
                    "package cache: promoted {promoted}, evicted {}",
                    evicted.len()
                );
            }
        })
        .await;
    }

    // 6. Upload artifacts on success. (The workspace is cleaned by `run_job`
    // on every exit path, including the `?` above.)
    if report.success {
        upload_artifacts(client, build_id, &pkgdir).await?;
    }

    Ok(report)
}

/// stdout and stderr differ in type but not in handling.
enum Either {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// How much output to accumulate before sending, and how long to hold a
/// partial batch. A build emits thousands of lines; one request per line would
/// swamp the server, and one request at the end would defeat the purpose.
const LOG_BATCH_BYTES: usize = 4096;
const LOG_BATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Forward a child stream to the build log, batched.
///
/// Batches flush on size or age, whichever comes first, and always on EOF — so
/// the last lines of a failed build, which are the ones that explain it, are
/// never left in the buffer.
async fn pump_output<R>(reader: R, client: Arc<WorkerClient>, build_id: i32)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut reader = tokio::io::BufReader::new(reader);
    let mut batch = String::new();
    let mut line = String::new();
    let mut last_flush = std::time::Instant::now();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                batch.push_str(&line);
                if batch.len() >= LOG_BATCH_BYTES || last_flush.elapsed() >= LOG_BATCH_INTERVAL {
                    log(&client, build_id, &batch).await;
                    batch.clear();
                    last_flush = std::time::Instant::now();
                }
            }
            // A read error ends the stream; the build's own exit status is
            // what determines success, so this is not escalated.
            Err(e) => {
                tracing::debug!("build {build_id}: reading output ended: {e}");
                break;
            }
        }
    }

    if !batch.is_empty() {
        log(&client, build_id, &batch).await;
    }
}

/// Spawn `makechrootpkg`, stream its output as logs, and poll for local
/// self-abort, a remote cancel request, and the build timeout — killing the
/// child in any of those cases.
///
/// The output really is streamed: stdout and stderr are piped and forwarded in
/// batches. Letting the child inherit the worker's stdio instead sends the
/// build's output to the worker's own container logs, where the user reading
/// the build page cannot see it.
async fn run_build(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    pkgdir: &Path,
    cache: &Cache,
    binds: &[(PathBuf, PathBuf)],
    cancel: &AtomicBool,
) -> Result<CompleteReport> {
    let build_id = job.build_id;
    let srcdest = cache.srcdest(&job.pkgbase);
    let argv = build::build_command(
        &cfg.chroot_dir,
        &format!("job-{build_id}"),
        binds,
        &job.build_flags,
        &cfg.build_user,
    );

    tracing::debug!("$ sudo {}", argv.join(" "));
    let spawn = |argv: &[String]| {
        let mut cmd = chroot::devtools(&argv[0]);
        cmd.args(&argv[1..]).current_dir(pkgdir).kill_on_drop(true);
        // Capture the build's output instead of letting it inherit the
        // worker's stdio. Without this the log a user sees ends at
        // "[worker] starting build" and everything makepkg prints — including
        // the error that explains a failure — only reaches the worker's
        // container logs.
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // devtools binds `$SRCDEST` into the chroot itself; without it set, it
        // falls back to the PKGBUILD directory and nothing is cached.
        if let Some(dir) = srcdest.as_deref() {
            cmd.env("SRCDEST", dir);
        }
        cmd.spawn()
    };

    let mut child = match spawn(&argv) {
        Ok(c) => c,
        Err(e) => {
            // Self-heal: a corrupt cache dir can break the first spawn; wipe and
            // retry once cold.
            cache.wipe_srcdest(&job.pkgbase);
            tracing::warn!("build spawn failed ({e}); retrying cold");
            spawn(&argv).context("spawning build")?
        }
    };

    // Measure the tree from here on. Started after the spawn so there is a pid
    // to root it at, and dropped with this function, which stops the sampler.
    let memory = child.id().map(crate::memory::PeakMemory::watching);

    // Poll for cancellation / timeout while the child runs. Local self-abort and
    // the build timeout are checked every 5s (cheap, in-process); the remote
    // cancel flag is polled less often (an HTTP round-trip) to avoid hammering
    // the server, and — now that the client carries connect/read timeouts — can
    // no longer block this loop indefinitely.
    let started = std::time::Instant::now();
    // Forward both streams to the build log. Held so they can be awaited after
    // the child exits, which is what guarantees the final lines are sent.
    let pumps = [
        child.stdout.take().map(Either::Out),
        child.stderr.take().map(Either::Err),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let client = Arc::clone(client);
        tokio::spawn(async move {
            match stream {
                Either::Out(r) => pump_output(r, client, build_id).await,
                Either::Err(r) => pump_output(r, client, build_id).await,
            }
        })
    })
    .collect::<Vec<_>>();

    let timeout = cfg.core.build_timeout;
    let remote_poll = Duration::from_secs(30);
    let mut last_remote_poll = std::time::Instant::now();
    let mut canceled = false;
    let mut timed_out = false;
    let status = loop {
        tokio::select! {
            res = child.wait() => break res.context("awaiting build")?,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                let hit_timeout = timeout > 0 && started.elapsed().as_secs() >= timeout;
                if hit_timeout {
                    timed_out = true;
                }
                if cancel.load(Ordering::SeqCst) {
                    canceled = true;
                } else if last_remote_poll.elapsed() >= remote_poll {
                    last_remote_poll = std::time::Instant::now();
                    if remote_cancel(client, build_id).await {
                        canceled = true;
                    }
                }
                if canceled || hit_timeout {
                    let _ = child.start_kill();
                    let status = child.wait().await.context("awaiting killed build")?;
                    break status;
                }
            }
        }
    };

    // Drain the output before reporting: the pumps hold whatever the build
    // printed last, which is exactly what a failure report needs.
    for pump in pumps {
        let _ = pump.await;
    }

    // Attached after the fact rather than threaded through every constructor:
    // how much a build used is orthogonal to why it ended, and a timeout or a
    // cancellation is exactly when the number is most worth having.
    let peak_memory_bytes = memory.as_ref().and_then(super::memory::PeakMemory::peak);
    let mut report = if timed_out {
        report::timeout_failure(started.elapsed().as_secs())
    } else {
        report::classify_exit(status, canceled)
    };
    report.peak_memory_bytes = peak_memory_bytes;
    Ok(report)
}
