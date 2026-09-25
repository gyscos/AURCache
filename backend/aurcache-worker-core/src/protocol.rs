//! Server interactions every executor performs around a build: streaming logs,
//! observing a remote cancel request, and uploading the results.

use anyhow::{Context, Result};
use aurcache_common::api::activity::Severity;
use std::path::Path;

use crate::artifacts;
use crate::client::WorkerClient;

/// Append a log line, ignoring transport errors.
///
/// Log delivery is best-effort by design: losing a line is a cosmetic problem,
/// while failing the build over one would turn a blip in the log channel into a
/// lost build.
pub async fn log(client: &WorkerClient, build_id: i32, text: &str) {
    if let Err(e) = client.append_log(build_id, text).await {
        tracing::debug!("log append failed: {e}");
    }
}

/// Tell the server about a problem, same best-effort delivery as [`log`]: this
/// is what lets it show up on the Logs page and the worker's own page instead
/// of only in this process's own journal, but it is never worth failing a
/// build over losing one.
pub async fn report_warning(client: &WorkerClient, build_id: Option<i32>, message: &str) {
    if let Err(e) = client
        .report_problem(Severity::Warning, build_id, message)
        .await
    {
        tracing::debug!("worker warning report failed: {e}");
    }
}

/// As [`report_warning`], for something the worker could not recover from.
pub async fn report_error(client: &WorkerClient, build_id: Option<i32>, message: &str) {
    if let Err(e) = client
        .report_problem(Severity::Error, build_id, message)
        .await
    {
        tracing::debug!("worker error report failed: {e}");
    }
}

/// True when the server has recorded a cancel request for this build.
pub async fn remote_cancel(client: &WorkerClient, build_id: i32) -> bool {
    client
        .job_status(build_id)
        .await
        .is_ok_and(|s| s.cancel_requested)
}

/// Upload every built artifact to the server's staging area.
///
/// Artifacts are streamed from disk rather than read into memory: package
/// files can be multi-gigabyte, and buffering them fully would triple the
/// worker's peak memory (one copy in the filesystem cache, one in the read,
/// one in the request body).
pub async fn upload_artifacts(client: &WorkerClient, build_id: i32, pkgdir: &Path) -> Result<()> {
    let found = artifacts::discover_artifacts(pkgdir);
    if found.is_empty() {
        anyhow::bail!("build produced no artifacts");
    }
    for path in found {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("artifact has no filename")?
            .to_string();
        // `O_NOFOLLOW`, and a regular file: see `discover_artifacts`. Checked
        // again at the open, which is the moment that decides what is read.
        let reader = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .await
            .with_context(|| format!("opening {} (a symlink is refused)", path.display()))?;
        let meta = reader
            .metadata()
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        if !meta.is_file() {
            anyhow::bail!(
                "{} is not a regular file; refusing to upload it",
                path.display()
            );
        }
        let len = Some(meta.len());
        log(client, build_id, &format!("[worker] uploading {name}\n")).await;
        client
            .upload_artifact(build_id, &name, reader, len)
            .await
            .with_context(|| format!("uploading {name}"))?;
    }
    Ok(())
}
