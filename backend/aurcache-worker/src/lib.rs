//! AURCache `devtools` chroot build worker.
//!
//! Builds each package in its own clean chroot via Arch's `devtools`. The
//! worker protocol itself — identity, enrollment, claiming, heartbeats — lives
//! in `aurcache-worker-core`; this crate is only the executor plus the
//! chroot-specific configuration, caches and credential handling.
//!
//! See `design/remote-workers.md`.

pub mod agent;
pub mod build;
pub mod cache;
pub mod cgroup;
pub mod chroot;
pub mod chroots;
pub mod config;
pub mod credentials;
pub mod executor;
pub mod job;
pub mod oneshot;
