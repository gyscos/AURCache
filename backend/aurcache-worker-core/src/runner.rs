//! The long-lived worker loop: claim jobs up to the concurrency limit, run each
//! in its own task with panic-safe completion, and heartbeat liveness. If the
//! server becomes unreachable for longer than the lease TTL, in-flight builds
//! self-abort (the server will have requeued them).

use anyhow::Result;
use aurcache_common::worker::{
    ClaimRequest, CompleteReport, Heartbeat, JobDescriptor, MirrorlistPreference,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

use crate::client::WorkerClient;
use crate::config::CoreConfig;
use crate::executor::Executor;
use crate::report;

/// Shared runtime state for the worker loop.
pub struct Runner<E: Executor> {
    cfg: Arc<CoreConfig>,
    client: Arc<WorkerClient>,
    executor: Arc<E>,
    /// Build ids currently executing (reported in each heartbeat).
    active: Mutex<HashMap<i32, Arc<AtomicBool>>>,
    /// Unix seconds of the last successful server contact.
    last_contact: AtomicU64,
    /// Bounds concurrent builds.
    permits: Arc<Semaphore>,
    /// Mirrorlists the server has sent, by architecture: `arch -> (checksum,
    /// content)`. The checksums go out with each claim so the server can skip
    /// resending what has not changed; the content is what a job then uses.
    ///
    /// Memory only. Losing it on restart costs one resend, and a restart is
    /// exactly when a worker should re-read the deployment's configuration
    /// anyway.
    mirrorlists: Mutex<BTreeMap<String, (String, String)>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Resolve `job.mirrorlist` to the content the build should actually use, and
/// keep `held` in step with what the server has said.
///
/// A free function rather than a method so the rule can be tested without a
/// server, a client or an executor -- and so there is exactly one copy of it.
///
/// Precedence: the worker's own mirrorlist, then the server's for this arch,
/// then nothing at all (the image's `/etc/pacman.d/mirrorlist`).
fn resolve_job_mirrorlist(
    local: Option<&str>,
    held: &mut BTreeMap<String, (String, String)>,
    job: &mut JobDescriptor,
) {
    if let Some(local) = local {
        job.mirrorlist = Some(local.to_string());
        return;
    }
    match (&job.mirrorlist, job.mirrorlist_unchanged) {
        // Sent afresh: use it, and remember it for the next claim.
        (Some(content), _) => {
            if let Some(checksum) = &job.mirrorlist_checksum {
                held.insert(job.arch.clone(), (checksum.clone(), content.clone()));
            }
        }
        // Withheld because we already hold it.
        (None, true) => job.mirrorlist = held.get(&job.arch).map(|(_, c)| c.clone()),
        // The server has none for this arch. Drop anything stale rather than
        // reuse it, or a mirrorlist removed on the server would live on here.
        (None, false) => {
            held.remove(&job.arch);
        }
    }
}

impl<E: Executor> Runner<E> {
    pub fn new(cfg: Arc<CoreConfig>, client: Arc<WorkerClient>, executor: Arc<E>) -> Arc<Self> {
        let concurrency = cfg.concurrency.max(1);
        Arc::new(Self {
            cfg,
            client,
            executor,
            active: Mutex::new(HashMap::new()),
            last_contact: AtomicU64::new(now_secs()),
            permits: Arc::new(Semaphore::new(concurrency)),
            mirrorlists: Mutex::new(BTreeMap::new()),
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
            "Worker '{}' online: native={:?} emulated={:?} concurrency={} executor={}",
            self.cfg.name,
            self.cfg.native_arches,
            self.cfg.emulated_arches,
            self.cfg.concurrency,
            self.executor.describe_self()
        );

        loop {
            // Acquire a permit before claiming so we never hold a job we can't run.
            let permit = Arc::clone(&self.permits).acquire_owned().await?;

            // Ask before claiming rather than after: a job we cannot start yet
            // is better left queued on the server, where it is visible and
            // costs nothing, than held here against a lease.
            if !self.executor.ready_for_work().await {
                drop(permit);
                tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval)).await;
                continue;
            }

            let claim = ClaimRequest {
                native_arches: self.cfg.native_arches.clone(),
                emulated_arches: self.cfg.emulated_arches.clone(),
                mirrorlist: self.mirrorlist_preference().await,
            };
            match self.client.claim(&claim).await {
                Ok(Some(mut job)) => {
                    self.mark_contact();
                    self.apply_mirrorlist(&mut job).await;
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

    /// What to tell the server about mirrorlists on the next claim.
    async fn mirrorlist_preference(&self) -> MirrorlistPreference {
        if self.cfg.mirrorlist.is_some() {
            return MirrorlistPreference::Local;
        }
        MirrorlistPreference::Server {
            checksums: self
                .mirrorlists
                .lock()
                .await
                .iter()
                .map(|(arch, (checksum, _))| (arch.clone(), checksum.clone()))
                .collect(),
        }
    }

    /// Resolve `job.mirrorlist` to the content the build should actually use.
    ///
    /// Done here so everything downstream -- the executor, the chroot -- keeps
    /// seeing one field holding the final content, and none of them has to know
    /// that it may have arrived on an earlier job.
    ///
    /// Precedence: this worker's own mirrorlist, then the server's for this
    /// arch, then nothing at all (the image's `/etc/pacman.d/mirrorlist`).
    async fn apply_mirrorlist(&self, job: &mut JobDescriptor) {
        let mut held = self.mirrorlists.lock().await;
        resolve_job_mirrorlist(self.cfg.mirrorlist.as_deref(), &mut held, job);
    }

    /// Execute a single job with panic-safe, always-emitted completion.
    async fn run_one(self: &Arc<Self>, job: JobDescriptor) {
        let build_id = job.build_id;
        let cancel = Arc::new(AtomicBool::new(false));
        self.active
            .lock()
            .await
            .insert(build_id, Arc::clone(&cancel));
        tracing::info!("Building {}", report::describe(&job));

        // Panic-wrap so a task panic still yields a terminal completion.
        let executor = Arc::clone(&self.executor);
        let client = Arc::clone(&self.client);
        let cancel_for_build = Arc::clone(&cancel);
        let result =
            tokio::spawn(async move { executor.run_job(client, job, cancel_for_build).await })
                .await;

        let report = match result {
            Ok(report) => report,
            Err(e) => report::setup_failure(format!("worker task panicked: {e}")),
        };

        self.report_completion(build_id, &report).await;
        self.active.lock().await.remove(&build_id);
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
                    // `{e:#}` for the cause chain, not just the outermost
                    // context: on its own "complete rejected" says nothing
                    // about *why* the server refused, and the status code is
                    // the whole difference between a lost lease and an ingest
                    // that cannot publish.
                    tracing::warn!("reporting completion for {build_id} failed: {e:#}");
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
                Ok(resp) => {
                    self.mark_contact();
                    // The server's abort list: any of our builds that were
                    // abandoned (lease lost) or cancelled by an operator. Set
                    // each flag; the build's next 5 s tick then aborts it.
                    if !resp.cancel.is_empty() {
                        let guard = self.active.lock().await;
                        for id in &resp.cancel {
                            if let Some(flag) = guard.get(id) {
                                flag.store(true, Ordering::SeqCst);
                                tracing::warn!("server asked to stop build #{id}; aborting");
                            }
                        }
                    }
                }
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

#[cfg(test)]
mod mirrorlist_tests {
    use aurcache_common::worker::{JobDescriptor, MirrorlistPreference};
    use std::collections::BTreeMap;

    use super::resolve_job_mirrorlist as apply;

    fn job(
        arch: &str,
        mirrorlist: Option<&str>,
        checksum: Option<&str>,
        unchanged: bool,
    ) -> JobDescriptor {
        JobDescriptor {
            build_id: 1,
            persistent_builddir: false,
            pkgbase: "hello".into(),
            arch: arch.into(),
            build_flags: vec![],
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: mirrorlist.map(ToString::to_string),
            mirrorlist_checksum: checksum.map(ToString::to_string),
            mirrorlist_unchanged: unchanged,
            pgp_keys: vec![],
        }
    }

    /// Content that arrives is used and remembered, so the next claim can
    /// advertise it and the server can skip resending.
    #[test]
    fn content_is_used_and_cached() {
        let mut held = BTreeMap::new();
        let mut j = job("x86_64", Some("Server = a\n"), Some("abc"), false);
        apply(None, &mut held, &mut j);
        assert_eq!(j.mirrorlist.as_deref(), Some("Server = a\n"));
        assert_eq!(
            held["x86_64"],
            ("abc".to_string(), "Server = a\n".to_string())
        );
    }

    /// Withheld content is filled in from the cache: this is what makes the
    /// build see a mirrorlist that arrived on an earlier job.
    #[test]
    fn unchanged_is_filled_from_the_cache() {
        let mut held = BTreeMap::new();
        held.insert(
            "x86_64".to_string(),
            ("abc".to_string(), "Server = a\n".to_string()),
        );
        let mut j = job("x86_64", None, None, true);
        apply(None, &mut held, &mut j);
        assert_eq!(j.mirrorlist.as_deref(), Some("Server = a\n"));
    }

    /// "The server has none" must clear the cache rather than reuse it, or a
    /// mirrorlist removed on the server would live on in every worker.
    #[test]
    fn no_server_mirrorlist_drops_the_cached_one() {
        let mut held = BTreeMap::new();
        held.insert(
            "x86_64".to_string(),
            ("abc".to_string(), "Server = a\n".to_string()),
        );
        let mut j = job("x86_64", None, None, false);
        apply(None, &mut held, &mut j);
        assert!(j.mirrorlist.is_none());
        assert!(held.is_empty(), "a stale entry would outlive the server's");
    }

    /// A worker's own mirrorlist wins over anything the server sends.
    #[test]
    fn a_local_mirrorlist_wins() {
        let mut held = BTreeMap::new();
        let mut j = job("x86_64", Some("Server = server\n"), Some("abc"), false);
        apply(Some("Server = mine\n"), &mut held, &mut j);
        assert_eq!(j.mirrorlist.as_deref(), Some("Server = mine\n"));
    }

    /// Caches are per architecture: a worker building two of them must not
    /// hand one arch's mirrors to the other.
    #[test]
    fn caches_do_not_cross_architectures() {
        let mut held = BTreeMap::new();
        let mut x = job("x86_64", Some("Server = x\n"), Some("cx"), false);
        apply(None, &mut held, &mut x);
        let mut a = job("aarch64", Some("Server = a\n"), Some("ca"), false);
        apply(None, &mut held, &mut a);
        assert_eq!(held["x86_64"].1, "Server = x\n");
        assert_eq!(held["aarch64"].1, "Server = a\n");
    }

    /// The default preference is "I hold nothing", which is what a worker
    /// predating the field sends, so it keeps receiving the content in full.
    #[test]
    fn the_default_preference_holds_nothing() {
        assert_eq!(
            MirrorlistPreference::default(),
            MirrorlistPreference::Server {
                checksums: BTreeMap::new()
            }
        );
    }
}
