//! End-to-end execution of a single claimed job: download the (server-patched)
//! source, prepare a per-package chroot copy, import trusted keys, build under a
//! resource-limited scope while streaming logs and honoring cancellation, then
//! upload artifacts. Returns a terminal [`CompleteReport`]; never panics past
//! the caller's wrapper.

use anyhow::{Context, Result};
use aurcache_types::worker::{CompleteReport, JobDescriptor};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::build;
use crate::cache::Cache;
use crate::chroot;
use crate::client::WorkerClient;
use crate::config::Config;

/// Run one job to completion and return its terminal report.
pub async fn run_job(
    cfg: &Config,
    client: &WorkerClient,
    job: JobDescriptor,
    cancel: Arc<AtomicBool>,
) -> CompleteReport {
    match run_job_inner(cfg, client, &job, &cancel).await {
        Ok(report) => report,
        Err(e) => {
            let msg = format!("build setup failed: {e:#}");
            let _ = client.append_log(job.build_id, &format!("\n[worker] {msg}\n")).await;
            build::setup_failure(msg)
        }
    }
}

async fn run_job_inner(
    cfg: &Config,
    client: &WorkerClient,
    job: &JobDescriptor,
    cancel: &Arc<AtomicBool>,
) -> Result<CompleteReport> {
    let build_id = job.build_id;
    let cache = Cache::new(&cfg.cache_dir, cfg.cache_max_size, cfg.cache_ttl);

    // Opportunistic cache GC (never blocks the build).
    cache.evict(std::slice::from_ref(&job.pkgbase));

    // 1. Fetch + extract source.
    log(client, build_id, "[worker] downloading source\n").await;
    let source = client.source(build_id).await.context("downloading source")?;
    let workdir = cfg.data_dir.join("work").join(build_id.to_string());
    let _ = std::fs::remove_dir_all(&workdir);
    let pkgdir = build::extract_source(&source, &workdir).context("extracting source")?;

    // 2. Write per-package configs + ensure base chroot.
    let cfg_dir = workdir.join("config");
    let (makepkg_conf, pacman_conf) = chroot::write_configs(
        &cfg_dir,
        &job.makepkg_conf,
        &job.pacman_conf,
        job.mirrorlist.as_deref(),
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
        return Ok(build::classify_exit_canceled());
    }

    // 4. Build under a resource-limited scope, honoring cancel.
    log(client, build_id, "[worker] starting build\n").await;
    let report = run_build(cfg, client, job, &pkgdir, &cache, cancel).await?;

    // 5. Upload artifacts on success.
    if report.success {
        upload_artifacts(client, build_id, &pkgdir).await?;
    }

    // 6. Cleanup the per-job workspace (keep caches + base chroot).
    let _ = std::fs::remove_dir_all(&workdir);
    Ok(report)
}

/// Spawn `makechrootpkg`, stream its output as logs, and poll for local
/// self-abort, a remote cancel request, and the build timeout — killing the
/// child in any of those cases.
async fn run_build(
    cfg: &Config,
    client: &WorkerClient,
    job: &JobDescriptor,
    pkgdir: &Path,
    cache: &Cache,
    cancel: &Arc<AtomicBool>,
) -> Result<CompleteReport> {
    let build_id = job.build_id;
    let srcdest = cache.srcdest(&job.pkgbase);
    let argv = build::build_command(
        &cfg.chroot_dir,
        &format!("job-{build_id}"),
        srcdest.as_deref(),
        &job.build_flags,
    );

    let spawn = |argv: &[String]| {
        let mut cmd = chroot::devtools(&argv[0]);
        cmd.args(&argv[1..]).current_dir(pkgdir).kill_on_drop(true);
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

    // Poll for cancellation / timeout while the child runs.
    let started = std::time::Instant::now();
    let timeout = cfg.build_timeout;
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
                if cancel.load(Ordering::SeqCst) || remote_cancel(client, build_id).await {
                    canceled = true;
                }
                if canceled || hit_timeout {
                    let _ = child.start_kill();
                    let status = child.wait().await.context("awaiting killed build")?;
                    break status;
                }
            }
        }
    };

    if timed_out {
        return Ok(build::timeout_failure(started.elapsed().as_secs()));
    }
    Ok(build::classify_exit(status, canceled))
}

/// Check the server for a cancel request (best-effort; failure = not canceled).
async fn remote_cancel(client: &WorkerClient, build_id: i32) -> bool {
    client
        .job_status(build_id)
        .await
        .map(|s| s.cancel_requested)
        .unwrap_or(false)
}

/// Upload every built artifact to the server's staging area.
async fn upload_artifacts(client: &WorkerClient, build_id: i32, pkgdir: &Path) -> Result<()> {
    let artifacts = build::discover_artifacts(pkgdir);
    if artifacts.is_empty() {
        anyhow::bail!("build produced no artifacts");
    }
    for path in artifacts {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("artifact has no filename")?
            .to_string();
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        log(client, build_id, &format!("[worker] uploading {name}\n")).await;
        client
            .upload_artifact(build_id, &name, bytes)
            .await
            .with_context(|| format!("uploading {name}"))?;
    }
    Ok(())
}

/// Append a log line, ignoring transport errors.
async fn log(client: &WorkerClient, build_id: i32, text: &str) {
    if let Err(e) = client.append_log(build_id, text).await {
        tracing::debug!("log append failed: {e}");
    }
}
