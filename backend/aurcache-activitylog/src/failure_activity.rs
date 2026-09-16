//! Things the server knows went wrong.
//!
//! Each of these sits beside an existing `warn!`/`error!` at a point where the
//! code already handles a failure: the journal line stays, and the log gains a
//! row an operator will actually come across. The severity is carried by the
//! [`ActivityType`](aurcache_db::activities::ActivityType), not by these
//! structs -- see `ActivityType::severity`.
//!
//! The reason is stored as the server rendered it at the time. A failure is
//! worth nothing without why it failed, and re-deriving the wording later from
//! a code would mean an entry that reads differently than it did when it
//! mattered.

use crate::activity_serializer::ActivitySerializer;
use aurcache_common::api::activity::ActivitySubject;
use serde::{Deserialize, Serialize};

/// A build that produced a package and never got it into the repository.
///
/// Distinct from a build that failed: this one worked, and the artifact exists.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublishFailedActivity {
    pub package: String,
    pub build: i32,
    pub reason: String,
}

impl ActivitySerializer for PublishFailedActivity {
    fn format(&self) -> String {
        format!(
            "publishing build #{} of {} failed: {}",
            self.build, self.package, self.reason
        )
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Package(self.package.clone()))
    }
}

/// Builds taken back from a worker that stopped answering.
///
/// One entry per pass rather than per build: the reaper finds them together,
/// and they have one cause.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerReapedActivity {
    /// Builds handed back to the queue for another attempt.
    pub retried: Vec<i32>,
    /// Builds that had no attempts left and were failed outright.
    pub failed: Vec<i32>,
}

impl ActivitySerializer for WorkerReapedActivity {
    fn format(&self) -> String {
        let mut parts = Vec::new();
        if !self.retried.is_empty() {
            parts.push(format!("requeued {}", builds(&self.retried)));
        }
        if !self.failed.is_empty() {
            parts.push(format!("gave up on {}", builds(&self.failed)));
        }
        format!(
            "a worker stopped answering: {}",
            // Nothing reaped is not written at all, so this is only reached
            // with at least one of the two lists populated.
            parts.join(", ")
        )
    }
}

/// `#1, #2 and #3`, because a bare list of numbers reads as a phone number.
fn builds(ids: &[i32]) -> String {
    let rendered: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
    match rendered.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// A pass of the version check that did not finish.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionCheckFailedActivity {
    pub reason: String,
}

impl ActivitySerializer for VersionCheckFailedActivity {
    fn format(&self) -> String {
        format!("the version check did not finish: {}", self.reason)
    }
}

/// A worker running something other than what its machine was configured with.
///
/// The counterpart of the flag on the Workers list: the same fact, somewhere
/// you come across it rather than somewhere you had to think to look.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerSettingRejectedActivity {
    pub worker: String,
    /// The settings it refused a value for.
    pub settings: Vec<String>,
}

impl ActivitySerializer for WorkerSettingRejectedActivity {
    fn format(&self) -> String {
        format!(
            "worker {} refused {} it was configured with: {}",
            self.worker,
            if self.settings.len() == 1 {
                "a value"
            } else {
                "values"
            },
            self.settings.join(", ")
        )
    }

    fn subject(&self) -> Option<ActivitySubject> {
        Some(ActivitySubject::Worker(self.worker.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_lists_read_as_a_sentence() {
        assert_eq!(builds(&[1]), "#1");
        assert_eq!(builds(&[1, 2]), "#1 and #2");
        assert_eq!(builds(&[1, 2, 3]), "#1, #2 and #3");
    }

    #[test]
    fn a_reaped_worker_names_what_became_of_each_build() {
        let text = WorkerReapedActivity {
            retried: vec![7],
            failed: vec![8, 9],
        }
        .format();
        assert!(text.contains("requeued #7"), "{text}");
        assert!(text.contains("gave up on #8 and #9"), "{text}");
    }

    /// One refused setting is "a value", several are "values" -- the log is
    /// prose, and prose that does not agree with itself reads as a bug.
    #[test]
    fn a_refusal_counts_its_settings() {
        let one = WorkerSettingRejectedActivity {
            worker: "builder-01".to_string(),
            settings: vec!["builddir_max_bytes".to_string()],
        }
        .format();
        assert!(one.contains("refused a value"), "{one}");

        let several = WorkerSettingRejectedActivity {
            worker: "builder-01".to_string(),
            settings: vec!["a".to_string(), "b".to_string()],
        }
        .format();
        assert!(several.contains("refused values"), "{several}");
        assert!(several.contains("a, b"), "{several}");
    }
}
