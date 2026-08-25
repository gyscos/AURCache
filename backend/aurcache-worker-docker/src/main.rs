//! AURCache legacy container build worker (deprecated).
//!
//! Enrolls and claims jobs exactly like `aurcache-worker` — same protocol, same
//! identity, same worker row — but builds each package in a container spawned
//! from the builder image instead of a `devtools` chroot.

use anyhow::{Result, anyhow};
use std::sync::Arc;

use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_core::{config::CoreConfig, enroll};
use aurcache_worker_docker::config::Config;
use aurcache_worker_docker::executor::DockerExecutor;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::from_env().ok_or_else(|| {
        anyhow!(
            "BUILD_ARTIFACT_DIR is not set. The compatibility builder binds a host directory \
             into each build container and cannot infer that path. Set it to the same value \
             your previous AURCache deployment used, or run `aurcache-worker` instead."
        )
    })?;
    let cfg = Arc::new(cfg);

    tracing::warn!(
        "The legacy container builder is deprecated and will be removed. It builds in a \
         reused container image rather than a clean chroot, and supports neither build \
         caches nor build credentials. See the Build Workers documentation for migrating \
         to the split server + worker setup."
    );

    let core: Arc<CoreConfig> = Arc::new(cfg.core.clone());
    let identity = Identity::load_or_create(&core.data_dir)?;
    let executor = Arc::new(DockerExecutor::connect(Arc::clone(&cfg)).await?);

    // The server may not be reachable yet (it may be starting alongside this
    // process) or may briefly go away. Retry rather than crashing.
    let client = loop {
        match enroll::ensure_enrolled(&core, &identity).await {
            Ok(client) => break client,
            Err(e) => {
                tracing::warn!(
                    "Enrollment not complete ({e:#}); retrying in {}s",
                    core.poll_interval
                );
                tokio::time::sleep(std::time::Duration::from_secs(core.poll_interval)).await;
            }
        }
    };

    Runner::new(core, Arc::new(client), executor).run().await
}
