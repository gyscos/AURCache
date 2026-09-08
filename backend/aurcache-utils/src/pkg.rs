use std::cmp::Ordering;
use std::collections::HashMap;
use std::str::FromStr;

use alpm_types::{Version, VersionRequirement};

pub use aurcache_deps::{parse_dep, satisfies_constraint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint(pub VersionRequirement);

impl Constraint {
    pub fn is_satisfied(&self, version: &Version) -> bool {
        self.0.is_satisfied_by(version)
    }
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Pacman-style version comparison using `alpm-types`.
///
/// Returns `None` when either side is not a valid alpm version, so callers have
/// to decide what an uncomparable pair means for them. Reporting that as
/// `Equal` would be worse than useless: "not newer" is exactly what suppresses
/// an update, so an unparseable version would silently freeze a package
/// forever.
pub fn vercmp(a: &str, b: &str) -> Option<Ordering> {
    match (Version::from_str(a), Version::from_str(b)) {
        (Ok(a), Ok(b)) => Some(a.cmp(&b)),
        _ => None,
    }
}

/// Insert a dependency constraint into a map. For constraints in the same
/// direction (both lower or both upper bounds) the stricter one is kept.
/// When directions differ (which would require a Range), the existing entry wins.
pub fn merge_constraint_into(
    constraints: &mut HashMap<String, Option<Constraint>>,
    name: &str,
    constraint: Option<Constraint>,
) -> anyhow::Result<()> {
    use alpm_types::VersionComparison::{Greater, GreaterOrEqual, Less, LessOrEqual};
    let merged = match (constraints.remove(name).flatten(), constraint) {
        (None, new) => new,
        (existing, None) => existing,
        (Some(lhs), Some(rhs)) => {
            let l = &lhs.0;
            let r = &rhs.0;
            Some(match (l.comparison, r.comparison) {
                (GreaterOrEqual | Greater, GreaterOrEqual | Greater) => {
                    if r.version > l.version {
                        rhs
                    } else {
                        lhs
                    }
                }
                (LessOrEqual | Less, LessOrEqual | Less) => {
                    if r.version < l.version {
                        rhs
                    } else {
                        lhs
                    }
                }
                _ => anyhow::bail!(
                    "conflicting constraints for '{name}': '{lhs}' and '{rhs}' bound in opposite directions"
                ),
            })
        }
    };
    constraints.insert(name.to_string(), merged);
    Ok(())
}

/// The dependencies a package declares.
///
/// The single answer to "what does this pkgbase need?". Both the add path and
/// the resync path used to derive this, from the same `PkgDeps`, with their
/// own near-copies of the merge loop — and only one of them kept the declared
/// order.
pub struct DependencySet {
    /// Names in declared order, deduplicated.
    ///
    /// The order is load-bearing: it decides the order dependencies are
    /// planned and therefore the order their builds are enqueued. Iterating
    /// `constraints` instead would vary between processes and make identical
    /// input produce different build orders.
    pub names: Vec<String>,
    /// The merged constraint for each name.
    pub constraints: HashMap<String, Option<Constraint>>,
}

impl DependencySet {
    /// Runtime and build-time dependencies together: AURCache needs both
    /// present before it can build anything.
    pub fn of(deps: &aurcache_deps::PkgDeps) -> anyhow::Result<Self> {
        Self::parse(deps.depends.iter().chain(deps.make_depends.iter()))
    }

    /// Parse and merge raw `name>=version` strings.
    pub fn parse<'a>(deps: impl Iterator<Item = &'a String>) -> anyhow::Result<Self> {
        let mut set = Self {
            names: Vec::new(),
            constraints: HashMap::new(),
        };
        for dep in deps {
            let (name, constraint) = parse_dep(dep);
            // The constraint map's keys are exactly the dependency set, so a
            // membership check there is the dedupe.
            if !set.constraints.contains_key(name) {
                set.names.push(name.to_string());
            }
            merge_constraint_into(&mut set.constraints, name, parse_dep_constraint(constraint))?;
        }
        Ok(set)
    }

    /// The constraint recorded for `name`, in the plain string form that both
    /// resolution and the `dependencies` rows use. Empty means unversioned.
    #[must_use]
    pub fn constraint_of(&self, name: &str) -> String {
        self.constraints
            .get(name)
            .cloned()
            .flatten()
            .map(|constraint| constraint.to_string())
            .unwrap_or_default()
    }

    /// Name/constraint pairs in declared order, ready to borrow
    /// [`aurcache_deps::Dependency`] values from.
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        self.names
            .iter()
            .map(|name| (name.clone(), self.constraint_of(name)))
            .collect()
    }
}

/// Borrow a [`DependencySet::to_pairs`] result as resolver input.
#[must_use]
pub fn as_dependencies(pairs: &[(String, String)]) -> Vec<aurcache_deps::Dependency<'_>> {
    pairs
        .iter()
        .map(|(name, constraint)| aurcache_deps::Dependency::new(name, constraint))
        .collect()
}

pub fn parse_dep_constraint(constraint: &str) -> Option<Constraint> {
    let constraint = constraint.trim();
    if constraint.is_empty() {
        return None;
    }
    VersionRequirement::from_str(constraint)
        .ok()
        .map(Constraint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vercmp_equal() {
        assert_eq!(vercmp("1.0", "1.0"), Some(Ordering::Equal));
        assert_eq!(vercmp("2.0.1", "2.0.1"), Some(Ordering::Equal));
        assert_eq!(vercmp("1.0-1", "1.0-1"), Some(Ordering::Equal));
    }

    #[test]
    fn test_vercmp_less() {
        assert_eq!(vercmp("1.0", "2.0"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0", "1.1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0", "1.0.1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0-1", "1.0-2"), Some(Ordering::Less));
    }

    #[test]
    fn test_vercmp_greater() {
        assert_eq!(vercmp("2.0", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.1", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.10", "1.9"), Some(Ordering::Greater));
    }

    #[test]
    fn test_vercmp_epoch() {
        assert_eq!(vercmp("1:1.0", "1:1.0"), Some(Ordering::Equal));
        assert_eq!(vercmp("2:1.0", "1:1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1:2.0", "1:1.0"), Some(Ordering::Greater));
    }

    #[test]
    fn test_vercmp_pkgrel() {
        assert_eq!(vercmp("1.0-1", "1.0"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.0", "1.0-1"), Some(Ordering::Less));
        assert_eq!(vercmp("1.0-2", "1.0-1"), Some(Ordering::Greater));
        assert_eq!(vercmp("1.0-1", "1.0-2"), Some(Ordering::Less));
    }

    /// An unparseable version is reported as uncomparable rather than as
    /// "equal", which callers would read as "not newer" and never update.
    #[test]
    fn test_vercmp_unparseable_is_uncomparable() {
        assert_eq!(vercmp("not a version!", "1.0"), None);
        assert_eq!(vercmp("1.0", "not a version!"), None);
    }

    #[test]
    fn test_merge_constraint_into_last_wins() {
        let mut constraints = HashMap::new();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=2.0")).unwrap();
        merge_constraint_into(&mut constraints, "glibc", parse_dep_constraint(">=3.0")).unwrap();

        assert_eq!(
            constraints
                .get("glibc")
                .cloned()
                .flatten()
                .map(|c| c.to_string())
                .unwrap_or_default(),
            ">=3.0"
        );
    }
}

/// The alpm architectures a package's configured platforms correspond to.
///
/// `packages.platforms` is a semicolon-delimited list of this project's
/// `Platform`; `.SRCINFO` is keyed by alpm's `SystemArchitecture`. Dependency
/// extraction needs the second, so the two have to be bridged somewhere.
///
/// Unparseable entries are skipped rather than failing: a bad platform string
/// should not stop a package's dependency graph from being computed for the
/// platforms that *are* valid.
#[must_use]
pub fn architectures_for_platforms(platforms: &str) -> Vec<alpm_types::SystemArchitecture> {
    use alpm_types::SystemArchitecture;
    use pacman_mirrors::platforms::Platform;

    Platform::parse_many(platforms)
        .filter_map(Result::ok)
        .map(|platform| match platform {
            Platform::X86_64 => SystemArchitecture::X86_64,
            Platform::Aarch64 => SystemArchitecture::Aarch64,
            Platform::Armv7h => SystemArchitecture::Armv7h,
        })
        .collect()
}

/// The platform names in a stored `platforms` string.
///
/// Dependency resolution scopes AURCache's own repository by these, since it
/// is stored one directory per platform and another platform's build cannot
/// satisfy this one's.
#[must_use]
pub fn platform_names(platforms: &str) -> Vec<String> {
    pacman_mirrors::platforms::Platform::parse_many(platforms)
        .filter_map(Result::ok)
        .map(|platform| platform.as_str().to_string())
        .collect()
}

#[cfg(test)]
mod architecture_tests {
    use super::architectures_for_platforms;
    use alpm_types::SystemArchitecture;

    #[test]
    fn each_platform_maps_to_its_alpm_architecture() {
        assert_eq!(
            architectures_for_platforms("x86_64;aarch64;armv7h"),
            vec![
                SystemArchitecture::X86_64,
                SystemArchitecture::Aarch64,
                SystemArchitecture::Armv7h,
            ]
        );
    }

    /// A package built only for aarch64 must not be described by x86_64's
    /// dependency list, which is what the hardcoded architecture used to do.
    #[test]
    fn a_single_platform_does_not_pull_in_x86_64() {
        assert_eq!(
            architectures_for_platforms("aarch64"),
            vec![SystemArchitecture::Aarch64]
        );
    }

    /// One bad entry must not lose the good ones.
    #[test]
    fn unparseable_platforms_are_skipped() {
        assert_eq!(
            architectures_for_platforms("x86_64;sparc;aarch64"),
            vec![SystemArchitecture::X86_64, SystemArchitecture::Aarch64]
        );
        assert!(architectures_for_platforms("").is_empty());
    }
}
