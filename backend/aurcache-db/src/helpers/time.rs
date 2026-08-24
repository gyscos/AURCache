//! Shared wall-clock helper.
//!
//! Lives here rather than in a dedicated utility crate because `aurcache-db` is
//! the lowest crate every server-side consumer already depends on. The
//! `aurcache-worker` binary deliberately does *not* depend on it (only its
//! tests do), so it keeps its own copy.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
///
/// A clock set before 1970 yields `0` rather than an error: every caller stores
/// this in a timestamp column where a bogus-but-ordered value is more useful
/// than failing the surrounding operation.
#[must_use]
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
