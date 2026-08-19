//! The long-lived worker loop: claim jobs up to the concurrency limit, run each
//! in its own task with panic-safe completion, and heartbeat liveness. If the
//! server becomes unreachable for longer than the lease TTL, in-flight builds
//! self-abort (the server will have requeued them).

use anyhow::Result;
use aurcache_types::worker::{ClaimRequest, CompleteReport, Heartbeat, JobDescriptor};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

use crate::build;
use crate::client::WorkerClient;
use crate::config::Config;
use crate::job;

/// Shared runtime state for the worker loop.
pub struct Runner {
    cfg: Arc<Config>,
    client: Arc<WorkerClient>,
    /// Build ids currently executing (reported in each heartbeat).
    active: Mutex<HashMap<i32, Arc<AtomicBool>>>,
    /// Pkgbases of currently-running jobs. Shared with each job so the cache
    /// garbage-collector never evicts a sibling job's in-progress `SRCDEST`
    /// when `concurrency > 1`.
    active_pkgbases: Arc<Mutex<HashSet<String>>>,
    /// Unix seconds of the last successful server contact.
    last_contact: AtomicU64,
    /// Bounds concurrent builds.
    permits: Arc<Semaphore>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Runner {
    pub fn new(cfg: Arc<Config>, client: Arc<WorkerClient>) -> Arc<Self> {
        let concurrency = cfg.concurrency.max(1);
        Arc::new(Self {
            cfg,
            client,
            active: Mutex::new(HashMap::new()),
            active_pkgbases: Arc::new(Mutex::new(HashSet::new())),
            last_contact: AtomicU64::new(now_secs()),
            permits: Arc::new(Semaphore::new(concurrency)),
        })
    }

    fn mark_contact(&self) {
        self.last_contact.store(now_secs(), Ordering::Relaxed);
    }

    /// Seconds since the last successful server contact.
    fn since_contact(&self) -> u64 {
        now_secs().saturating_sub(self.last_contact.load(Ordering::Relaxed))
    }

    /// Run forever: heartbeat in the background, claim + build in the foreground.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let hb = Arc::clone(&self);
        tokio::spawn(async move { hb.heartbeat_loop().await });

        tracing::info!(
            "Worker '{}' online: native={:?} emulated={:?} concurrency={}",
            self.cfg.name,
            self.cfg.native_arches,
            self.cfg.emulated_arches,
            self.cfg.concurrency
        );

        loop {
            // Acquire a permit before claiming so we never hold a job we can't run.
            let permit = Arc::clone(&self.permits).acquire_owned().await?;

            let claim = ClaimRequest {
                native_arches: self.cfg.native_arches.clone(),
                emulated_arches: self.cfg.emulated_arches.clone(),
            };
            match self.client.claim(&claim).await {
                Ok(Some(job)) => {
                    self.mark_contact();
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        this.run_one(job).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    self.mark_contact();
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval)).await;
                }
                Err(e) => {
                    tracing::warn!("claim failed: {e}");
                    drop(permit);
                    tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval)).await;
                }
            }
        }
    }

    /// Execute a single job with panic-safe, always-emitted completion.
    async fn run_one(self: &Arc<Self>, job: JobDescriptor) {
        let build_id = job.build_id;
        let pkgbase = job.pkgbase.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.lock().await.insert(build_id, Arc::clone(&cancel));
        // Register before building so this job's own SRCDEST (and every sibling's)
        // is protected from the cache GC that runs at each job's start.
        self.active_pkgbases.lock().await.insert(pkgbase.clone());
        tracing::info!("Building {}", build::describe(&job));

        // Panic-wrap so a task panic still yields a terminal completion.
        let this = Arc::clone(self);
        let cancel_for_build = Arc::clone(&cancel);
        let job_for_build = job.clone();
        let active_pkgbases = Arc::clone(&self.active_pkgbases);
        let result = tokio::spawn(async move {
            job::run_job(
                &this.cfg,
                &this.client,
                job_for_build,
                cancel_for_build,
                active_pkgbases,
            )
            .await
        })
        .await;

        let report = match result {
            Ok(report) => report,
            Err(e) => build::setup_failure(format!("worker task panicked: {e}")),
        };

        self.report_completion(build_id, &report).await;
        self.active.lock().await.remove(&build_id);
        self.active_pkgbases.lock().await.remove(&pkgbase);
    }

    /// Send the terminal completion, retrying briefly so a transient network
    /// blip does not drop the report (which would force a lease-expiry requeue).
    async fn report_completion(&self, build_id: i32, report: &CompleteReport) {
        for attempt in 0..5 {
            match self.client.complete(build_id, report).await {
                Ok(()) => {
                    self.mark_contact();
                    if report.success {
                        tracing::info!("Build {build_id} completed successfully");
                    } else {
                        tracing::warn!(
                            "Build {build_id} failed: {}",
                            report.reason.as_deref().unwrap_or("unknown")
                        );
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!("reporting completion for {build_id} failed: {e}");
                    tokio::time::sleep(Duration::from_secs(2 * (attempt + 1))).await;
                }
            }
        }
        tracing::error!("gave up reporting completion for {build_id}; server will requeue");
    }

    /// Background heartbeat + self-abort watchdog.
    async fn heartbeat_loop(self: Arc<Self>) {
        let interval = Duration::from_secs(self.cfg.heartbeat_interval.max(1));
        loop {
            tokio::time::sleep(interval).await;

            let active_build_ids: Vec<i32> = {
                let guard = self.active.lock().await;
                guard.keys().copied().collect()
            };

            let hb = Heartbeat {
                active_build_ids: active_build_ids.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            };
            match self.client.heartbeat(&hb).await {
                Ok(()) => self.mark_contact(),
                Err(e) => tracing::warn!("heartbeat failed: {e}"),
            }

            // Self-abort: if the server has been unreachable past the lease TTL,
            // the builds we hold have already been requeued — stop them to avoid
            // a zombie upload.
            if !active_build_ids.is_empty() && self.since_contact() > self.cfg.lease_ttl {
                tracing::error!(
                    "server unreachable for {}s (> lease {}s); self-aborting {} build(s)",
                    self.since_contact(),
                    self.cfg.lease_ttl,
                    active_build_ids.len()
                );
                let guard = self.active.lock().await;
                for flag in guard.values() {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }
    }
}
