//! Non-interactive worker enrollment (auto-approval) policy.
//!
//! A newly registered worker is `pending` and normally waits for an admin to
//! approve it. Three env-driven modes let common deployments skip that click
//! (see design doc §"One worker command, four enrollment paths"); the fourth
//! path is the default interactive approval, which needs no code here.
//!
//! Modes, highest trust first:
//! 1. **Capability enrollment volume** (`AURCACHE_ENROLLMENT_DIR`): the worker
//!    drops `<fingerprint>.csr` (public only) into a volume the backend mounts
//!    read-only. Presence of that file proves shared-volume access → approve.
//! 2. **Pre-approved fingerprints** (`AURCACHE_PREAPPROVED_WORKERS`): a
//!    comma-separated list of `fingerprint[:name[:arches]]` entries.
//! 3. **Shared enrollment token** (`AURCACHE_ENROLLMENT_TOKEN`): fallback for
//!    setups without a shared volume; the worker presents the token.

use std::env;
use std::path::PathBuf;

/// Pure decision core (env/filesystem inputs passed in), so it is unit-testable.
#[must_use]
pub fn eval_auto_approve(
    fingerprint: &str,
    provided_token: Option<&str>,
    expected_token: Option<&str>,
    preapproved_fingerprints: &[String],
    enrollment_dir_has_csr: bool,
) -> bool {
    if enrollment_dir_has_csr {
        return true;
    }
    if preapproved_fingerprints.iter().any(|f| f == fingerprint) {
        return true;
    }
    matches!(
        (provided_token, expected_token),
        (Some(p), Some(e)) if !e.is_empty() && p == e
    )
}

/// Parse `AURCACHE_PREAPPROVED_WORKERS` (`fp[:name[:arches]],…`) into the set of
/// pre-approved fingerprints (the first `:`-separated field of each entry).
#[must_use]
pub fn parse_preapproved(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| entry.split(':').next())
        .map(|fp| fp.trim().to_string())
        .filter(|fp| !fp.is_empty())
        .collect()
}

/// Evaluate the auto-approval policy from the environment for a given worker.
///
/// Reads `AURCACHE_ENROLLMENT_DIR`, `AURCACHE_PREAPPROVED_WORKERS`, and
/// `AURCACHE_ENROLLMENT_TOKEN`. Filesystem access for the enrollment volume is
/// best-effort: a missing/unreadable dir simply means that mode doesn't match.
#[must_use]
pub fn auto_approve_from_env(fingerprint: &str, provided_token: Option<&str>) -> bool {
    let has_csr = env::var("AURCACHE_ENROLLMENT_DIR")
        .ok()
        .map(|dir| {
            PathBuf::from(dir)
                .join(format!("{fingerprint}.csr"))
                .is_file()
        })
        .unwrap_or(false);

    let preapproved = env::var("AURCACHE_PREAPPROVED_WORKERS")
        .map(|raw| parse_preapproved(&raw))
        .unwrap_or_default();

    let expected_token = env::var("AURCACHE_ENROLLMENT_TOKEN").ok();

    eval_auto_approve(
        fingerprint,
        provided_token,
        expected_token.as_deref(),
        &preapproved,
        has_csr,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_volume_grants_approval() {
        assert!(eval_auto_approve("fp", None, None, &[], true));
    }

    #[test]
    fn preapproved_fingerprint_grants_approval() {
        let pre = vec!["aaa".to_string(), "bbb".to_string()];
        assert!(eval_auto_approve("bbb", None, None, &pre, false));
        assert!(!eval_auto_approve("ccc", None, None, &pre, false));
    }

    #[test]
    fn matching_token_grants_approval() {
        assert!(eval_auto_approve(
            "fp",
            Some("s3cret"),
            Some("s3cret"),
            &[],
            false
        ));
        assert!(!eval_auto_approve(
            "fp",
            Some("wrong"),
            Some("s3cret"),
            &[],
            false
        ));
        // No configured token -> a provided token never approves.
        assert!(!eval_auto_approve("fp", Some("s3cret"), None, &[], false));
        // Empty configured token is treated as unset.
        assert!(!eval_auto_approve("fp", Some(""), Some(""), &[], false));
    }

    #[test]
    fn no_mode_matches_stays_pending() {
        assert!(!eval_auto_approve("fp", None, None, &[], false));
    }

    #[test]
    fn parse_preapproved_extracts_fingerprints() {
        let parsed = parse_preapproved("aa:worker-a:x86_64, bb , cc:worker-c");
        assert_eq!(parsed, vec!["aa", "bb", "cc"]);
        assert!(parse_preapproved("").is_empty());
    }
}
