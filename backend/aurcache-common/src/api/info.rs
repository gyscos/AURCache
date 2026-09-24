//! What the server reports about itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The running server's own version: the release, plus the commit when the
/// build is not exactly that release (see [`crate::version`]).
///
/// The bundled UI shows this rather than its own crate version, so the two
/// never disagree about what is running.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// Whatever the server logs at startup, e.g. `0.5.0+g8afa04a.dirty`.
    pub version: String,
}
