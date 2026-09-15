//! Shapes and helpers shared across the workspace: the API types the server,
//! CLI and browser frontend all speak, plus the small pieces of behaviour that
//! would otherwise be duplicated or force a heavyweight dependency.
//!
//! Everything here is either dependency-free or behind a feature, so a wasm
//! consumer can take `default-features = false` and still get the types.

pub mod api;
pub mod build_state;
/// Build-queue messages. Requires the `db` feature: they carry database models.
#[cfg(feature = "db")]
pub mod builder;
/// Filesystem helpers. Requires the `fs` feature: `std::fs` is meaningless in
/// a browser, so wasm consumers leave it off.
#[cfg(feature = "fs")]
pub mod fs;
pub mod ports;
pub mod repo;
pub mod settings;
pub mod source;
/// Sizes and durations as configuration writes them (`40G`, `3h`).
pub mod units;
pub mod worker;
