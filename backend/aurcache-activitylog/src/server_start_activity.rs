use crate::activity_serializer::ActivitySerializer;
use serde::{Deserialize, Serialize};

/// The server process starting.
///
/// Carries the version, which is the reason the entry is worth having: it is
/// what turns "the log looks different after Tuesday" into "the deploy landed
/// on Tuesday".
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerStartActivity {
    pub version: String,
}

impl ActivitySerializer for ServerStartActivity {
    fn format(&self) -> String {
        format!("AURCache {} started", self.version)
    }
}
