//! Terminal build reports, shared by every executor.
//!
//! A worker must always send exactly one [`CompleteReport`] per claimed job —
//! staying silent forces the server to wait out the lease and requeue. These
//! constructors are the vocabulary for that report; how the build itself ran is
//! the executor's business.

use aurcache_common::worker::{CompleteReport, JobDescriptor};
use std::collections::BTreeMap;
use std::process::ExitStatus;

/// Map a build process exit status into a terminal report.
#[must_use]
pub fn classify_exit(status: ExitStatus, canceled: bool) -> CompleteReport {
    if canceled {
        return canceled_report(status.code());
    }
    if status.success() {
        return CompleteReport {
            success: true,
            exit_code: Some(0),
            reason: None,
            canceled: false,
            peak_memory_bytes: None,
            vcs_commits: BTreeMap::new(),
        };
    }
    let code = status.code();
    let reason = match code {
        Some(137) => "build killed (OOM, exit 137)".to_string(),
        Some(124) => "build timed out (exit 124)".to_string(),
        Some(c) => format!("build failed (exit {c})"),
        None => "build terminated by signal".to_string(),
    };
    CompleteReport {
        success: false,
        exit_code: code,
        reason: Some(reason),
        canceled: false,
        peak_memory_bytes: None,
        vcs_commits: BTreeMap::new(),
    }
}

/// A terminal report for a failure before or around the build itself
/// (source download, workspace preparation, artifact upload).
#[must_use]
pub fn setup_failure(reason: impl std::fmt::Display) -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code: None,
        reason: Some(reason.to_string()),
        canceled: false,
        peak_memory_bytes: None,
        vcs_commits: BTreeMap::new(),
    }
}

/// A terminal report for a build aborted before it started (cancel observed
/// during setup, so there is no process exit status).
#[must_use]
pub fn classify_exit_canceled() -> CompleteReport {
    canceled_report(None)
}

/// The shared "build canceled" shape, with whatever exit code is available.
fn canceled_report(exit_code: Option<i32>) -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code,
        reason: Some("build canceled".to_string()),
        canceled: true,
        peak_memory_bytes: None,
        vcs_commits: BTreeMap::new(),
    }
}

/// A terminal report for a build the worker killed after exceeding its timeout.
#[must_use]
pub fn timeout_failure(secs: u64) -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code: Some(124),
        reason: Some(format!("build timed out after {secs}s")),
        canceled: false,
        peak_memory_bytes: None,
        vcs_commits: BTreeMap::new(),
    }
}

/// Short human label for a job, used in worker logs.
#[must_use]
pub fn describe(job: &JobDescriptor) -> String {
    format!("{} ({})", job.pkgbase, job.arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_success() {
        let r = classify_exit(fake_status(0), false);
        assert!(r.success);
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn classifies_oom_as_terminal_failure() {
        let r = classify_exit(fake_status(137), false);
        assert!(!r.success);
        assert_eq!(r.exit_code, Some(137));
        assert!(r.reason.unwrap().contains("OOM"));
        assert!(!r.canceled);
    }

    #[test]
    fn classifies_cancel() {
        let r = classify_exit(fake_status(1), true);
        assert!(!r.success);
        assert!(r.canceled);
    }

    #[cfg(unix)]
    fn fake_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}
