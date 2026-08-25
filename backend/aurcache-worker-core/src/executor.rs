//! The seam between the worker protocol and how a package actually gets built.
//!
//! [`Runner`](crate::runner::Runner) owns everything that makes a worker a
//! worker — claiming within a concurrency limit, heartbeating, self-aborting a
//! build it can no longer report, and always emitting exactly one terminal
//! report. An [`Executor`] owns only the part that differs between build
//! strategies: turning one [`JobDescriptor`] into built packages.
//!
//! There is deliberately no runtime selection here. Each executor lives in its
//! own crate with its own binary, so neither has to know the other exists and
//! retiring one is a deletion rather than a refactor.

use aurcache_types::worker::{CompleteReport, JobDescriptor};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::client::WorkerClient;

/// Builds one claimed job to completion.
pub trait Executor: Send + Sync + 'static {
    /// Run a job and return its terminal report.
    ///
    /// Implementations must not panic — the runner wraps the call and reports a
    /// panic as a failure, but a clean report carries a far better message.
    /// `cancel` is set when the worker has decided to abandon the build (the
    /// server became unreachable past the lease); implementations should stop
    /// promptly and return [`classify_exit_canceled`](crate::report::classify_exit_canceled).
    fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: JobDescriptor,
        cancel: Arc<AtomicBool>,
    ) -> impl Future<Output = CompleteReport> + Send;

    /// One-line summary of the executor, logged when the worker comes online,
    /// so a worker's logs say which build strategy produced its packages.
    fn describe_self(&self) -> String;
}
