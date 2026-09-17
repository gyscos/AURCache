use anyhow::anyhow;
use sea_orm::DeriveValueType;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize, DeriveValueType)]
#[sea_orm(value_type = "String")]
pub enum Platform {
    X86_64,
    Aarch64,
    Armv7h,
}

impl Platform {
    /// Every platform AURCache builds for.
    ///
    /// Here rather than spelled out at each use, so that adding one is a change
    /// in this file alone -- the callers that iterate architectures (mirrorlist
    /// configuration, official-repo caches) then pick it up for free.
    pub const ALL: [Self; 3] = [Self::X86_64, Self::Aarch64, Self::Armv7h];

    /// Returns the string representation of the platform.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Armv7h => "armv7h",
        }
    }

    /// Iterate over a semicolon-separated list of platform names.
    ///
    /// Each entry is trimmed, empty entries are skipped, and unknown platform
    /// names produce an error in the returned `Result`.
    pub fn parse_many(platforms: &str) -> impl Iterator<Item = anyhow::Result<Self>> + '_ {
        platforms
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Self::from_str(s).map_err(|_| anyhow!("Invalid platform '{s}'")))
    }

    /// Canonical `;`-joined form: sorted and deduplicated, so the same set
    /// always stores (and compares) the same way regardless of request order.
    /// Without this, `["aarch64", "x86_64"]` and `["x86_64", "aarch64"]` store
    /// differently and every re-listing of the same set reads as a change.
    #[must_use]
    pub fn join_canonical(platforms: &[Self]) -> String {
        let mut names: Vec<&'static str> = platforms.iter().map(Self::as_str).collect();
        names.sort_unstable();
        names.dedup();
        names.join(";")
    }

    /// Canonical form of an already-stored `;`-joined list: sorted and
    /// deduplicated without validating, so rows written before
    /// [`Self::join_canonical`] (request order, possibly with repeats) still
    /// compare equal to the same set. Unknown segments are kept as-is — this
    /// is a comparison helper, not validation.
    #[must_use]
    pub fn canonicalize_joined(raw: &str) -> String {
        let mut parts: Vec<&str> = raw.split(';').filter(|s| !s.is_empty()).collect();
        parts.sort_unstable();
        parts.dedup();
        parts.join(";")
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Implements conversion from a &str to a Platform.
/// The error [`Platform::from_str`] returns.
///
/// A named type rather than a `&'static str`: sea-orm 2.0 requires the
/// conversion error behind `DeriveValueType` to be a real `std::error::Error`,
/// and carrying the rejected value means the message can say what was wrong
/// rather than only that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePlatformError(String);

impl std::fmt::Display for ParsePlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown platform '{}'", self.0)
    }
}

impl std::error::Error for ParsePlatformError {}

impl FromStr for Platform {
    type Err = ParsePlatformError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            "armv7h" => Ok(Self::Armv7h),
            _ => Err(ParsePlatformError(s.to_string())),
        }
    }
}

/// A wrapper type that can be iterated over to yield all Platform variants.
pub struct Platforms;

impl IntoIterator for Platforms {
    type Item = Platform;
    type IntoIter = std::array::IntoIter<Platform, 3>;

    fn into_iter(self) -> Self::IntoIter {
        [Platform::X86_64, Platform::Aarch64, Platform::Armv7h].into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Platform;

    /// The same set in any order — or with repeats — stores one way, so
    /// re-listing it never reads as a change.
    #[test]
    fn canonical_join_is_order_and_duplicate_free() {
        use Platform::{Aarch64, Armv7h, X86_64};
        assert_eq!(
            Platform::join_canonical(&[Aarch64, X86_64]),
            Platform::join_canonical(&[X86_64, Aarch64, X86_64]),
        );
        assert_eq!(
            Platform::join_canonical(&[Armv7h, Aarch64, X86_64, Aarch64]),
            "aarch64;armv7h;x86_64"
        );
        assert_eq!(Platform::join_canonical(&[]), "");
    }

    /// Stored lists from before canonical storage still match the same set,
    /// whatever order (or repeats) they were written in.
    #[test]
    fn canonicalize_joined_matches_join_canonical() {
        use Platform::{Aarch64, X86_64};
        let canonical = Platform::join_canonical(&[X86_64, Aarch64]);
        assert_eq!(Platform::canonicalize_joined("x86_64;aarch64"), canonical);
        assert_eq!(
            Platform::canonicalize_joined("aarch64;x86_64;x86_64"),
            canonical
        );
        assert_eq!(Platform::canonicalize_joined(""), "");
    }
}
