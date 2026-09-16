use crate::activity_serializer::ActivitySerializer;
use aurcache_common::api::activity::ActivitySubject;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PackageAddActivity {
    pub package: String,
}

impl ActivitySerializer for PackageAddActivity {
    fn format(&self) -> String {
        format!("added package {}", self.package)
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Package(self.package.clone()))
    }
}
