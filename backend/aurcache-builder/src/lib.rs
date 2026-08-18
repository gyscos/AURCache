//! Build-queue coordination for the remote-worker model.
//!
//! The server no longer builds packages itself — remote workers claim
//! `ENQUEUED` builds by polling. This crate only keeps the lightweight
//! coordinator that seeds buildable packages on startup and turns user cancel
//! requests into a database state the worker observes via `/jobs/{id}/status`.

pub mod init;
