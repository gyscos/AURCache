//! Shared machinery for AURCache build workers.
//!
//! Everything that makes a process a *worker* lives here: identity, enrollment
//! over mTLS, the job protocol, and the claim/heartbeat/report loop. What it
//! does not contain is any notion of how a package is built — that is an
//! [`Executor`](executor::Executor), implemented once per build strategy in its
//! own crate.
//!
//! `aurcache-worker` is the supported executor: each package is built in a
//! `devtools` chroot. `aurcache-worker-docker` reproduces the pre-worker
//! behaviour of spawning a build container, and exists only so single-container
//! deployments keep working across the upgrade.
//!
//! See `design/remote-workers.md`.

pub mod artifacts;
pub mod client;
pub mod config;
pub mod enroll;
pub mod executor;
pub mod identity;
pub mod protocol;
pub mod repo;
pub mod report;
pub mod runner;
