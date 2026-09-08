//! Pacman-style version comparison.
//!
//! This lives at the bottom of the crate stack because two layers need it and
//! neither can see the other's copy: dependency resolution here has to know
//! whether a repository entry actually satisfies a constraint, and the build
//! queue in `aurcache-utils` has to know whether a dependency's latest build
//! does. `aurcache-utils` re-exports this rather than keeping a second
//! implementation.

use std::str::FromStr;

use alpm_types::{Version, VersionRequirement};

/// Whether `version` satisfies `constraint`, a pacman constraint string such
/// as `">=2.0"`.
///
/// An empty constraint is satisfied by any version -- that is how an
/// unversioned `depends` entry is stored. A version or constraint that
/// `alpm-types` cannot parse is treated as *not* satisfied: refusing to
/// promote a build is recoverable, promoting one against a version nobody
/// could parse is not.
#[must_use]
pub fn satisfies_constraint(version: &str, constraint: &str) -> bool {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return true;
    }
    let Ok(version) = Version::from_str(version) else {
        return false;
    };
    let Ok(requirement) = VersionRequirement::from_str(constraint) else {
        return false;
    };
    requirement.is_satisfied_by(&version)
}

#[cfg(test)]
mod tests {
    use super::satisfies_constraint;

    #[test]
    fn test_satisfies_constraint() {
        assert!(satisfies_constraint("2.0", ">=1.0"));
        assert!(satisfies_constraint("2.0", ">=2.0"));
        assert!(!satisfies_constraint("1.0", ">=2.0"));
        assert!(satisfies_constraint("1.0", "<=2.0"));
        assert!(satisfies_constraint("2.0", "<=2.0"));
        assert!(!satisfies_constraint("3.0", "<=2.0"));
        assert!(satisfies_constraint("1.5", "=1.5"));
        assert!(!satisfies_constraint("1.6", "=1.5"));
        assert!(satisfies_constraint("2.0", ">1.0"));
        assert!(!satisfies_constraint("1.0", ">1.0"));
        assert!(satisfies_constraint("1.0", "<2.0"));
        assert!(!satisfies_constraint("2.0", "<2.0"));
        assert!(satisfies_constraint("2.0", ""));
        assert!(satisfies_constraint("2.0", ">=1.0-2"));
    }

    /// A repository entry whose version is unparseable must not be allowed to
    /// answer a versioned dependency.
    #[test]
    fn unparseable_versions_do_not_satisfy() {
        assert!(!satisfies_constraint("not a version", ">=1.0"));
        assert!(!satisfies_constraint("1.0", "not a constraint"));
        // ...but an unversioned dependency is satisfied regardless, since
        // nothing about the version was ever asked.
        assert!(satisfies_constraint("not a version", ""));
    }
}
