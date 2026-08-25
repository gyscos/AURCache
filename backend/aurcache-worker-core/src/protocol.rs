//! Server interactions every executor performs around a build: streaming logs,
//! observing a remote cancel request, and uploading the results.

use anyhow::{Context, Result};
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

/// True when the server has recorded a cancel request for this build.
pub async fn remote_cancel(client: &WorkerClient, build_id: i32) -> bool {
    client
        .job_status(build_id)
        .await
        .is_ok_and(|s| s.cancel_requested)
}

/// Upload every built artifact to the server's staging area.
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
