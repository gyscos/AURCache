//! AURCache remote build worker library.
//!
//! Exposes the worker's internal modules so they can be reused by the binary
//! (`main.rs`) and by integration tests (e.g. the hermetic fake-worker protocol
//! test in `tests/`). See `design/remote-workers.md`.

pub mod build;
pub mod cache;
pub mod chroot;
pub mod client;
pub mod config;
pub mod credentials;
pub mod enroll;
pub mod identity;
pub mod job;
pub mod oneshot;
pub mod runner;
