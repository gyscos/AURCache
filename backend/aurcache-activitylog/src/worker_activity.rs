//! What happened to a build worker.
//!
//! One struct per event rather than one carrying a verb: the text is rendered
//! from the stored row, and an entry written last year should still read the
//! same after the vocabulary around it changes.
//!
//! The name rather than the fingerprint, because the log is prose. A name is
//! not unique -- see `design/worker-configuration.md` -- but an entry is a
//! record of a moment, not a link to a row, and the moment had a name.

use crate::activity_serializer::ActivitySerializer;
use aurcache_common::api::activity::ActivitySubject;
use serde::{Deserialize, Serialize};

/// A machine joining the fleet for the first time.
///
/// Only the first time: a worker registers on every startup, so logging each
/// one would turn an ordinary restart -- or a crash loop -- into a log nobody
/// can read past.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerEnrollActivity {
    pub worker: String,
}

impl ActivitySerializer for WorkerEnrollActivity {
    fn format(&self) -> String {
        format!("worker {} enrolled", self.worker)
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Worker {
            name: self.worker.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerApproveActivity {
    pub worker: String,
}

impl ActivitySerializer for WorkerApproveActivity {
    fn format(&self) -> String {
        format!("approved worker {}", self.worker)
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Worker {
            name: self.worker.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerRevokeActivity {
    pub worker: String,
}

impl ActivitySerializer for WorkerRevokeActivity {
    fn format(&self) -> String {
        format!("revoked worker {}", self.worker)
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Worker {
            name: self.worker.clone(),
        })
    }
}
