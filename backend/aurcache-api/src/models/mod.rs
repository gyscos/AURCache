//! Server-side view of the API shapes.
//!
//! The shapes themselves live in `aurcache-types` so the HTTP client and a
//! browser frontend use the same structs rather than hand-mirrored copies.
//! Only what is genuinely server-only — the request guard — is defined here.

pub mod authenticated;

pub use aurcache_types::api::{aur, builds, package, settings, stats};
