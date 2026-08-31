//! The `devtools` chroot executor.
//!
//! Owns the state that spans concurrent builds on this worker — currently the
//! set of pkgbases with a live `SRCDEST`, which the cache garbage-collector
//! must not evict out from under a sibling build.

use aurcache_common::worker::{CompleteReport, JobDescriptor};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::Executor;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::job;

/// Builds each package in its own `devtools` chroot copy.
pub struct ChrootExecutor {
    cfg: Arc<Config>,
    /// Pkgbases of currently-running jobs. Shared with each job so the cache
    /// garbage-collector never evicts a sibling job's in-progress `SRCDEST`
    /// when `concurrency > 1`.
    active_pkgbases: Arc<Mutex<HashSet<String>>>,
}

impl ChrootExecutor {
    #[must_use]
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            active_pkgbases: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

impl Executor for ChrootExecutor {
    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: JobDescriptor,
        cancel: Arc<AtomicBool>,
    ) -> CompleteReport {
        let pkgbase = job.pkgbase.clone();
        // Register before building so this job's own SRCDEST (and every
        // sibling's) is protected from the cache GC that runs at each job's
        // start.
        self.active_pkgbases.lock().await.insert(pkgbase.clone());

        let report = job::run_job(
            &self.cfg,
            &client,
            job,
            cancel,
            Arc::clone(&self.active_pkgbases),
        )
        .await;

        self.active_pkgbases.lock().await.remove(&pkgbase);
        report
    }

    fn describe_self(&self) -> String {
        format!("devtools chroot ({})", self.cfg.chroot_dir.display())
    }
}
