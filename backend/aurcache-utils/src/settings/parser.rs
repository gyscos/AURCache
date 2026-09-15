use std::str::FromStr;

pub trait ParseSetting: Sized + Clone {
    fn parse_setting(s: &str) -> Result<Self, String>;
}
macro_rules! impl_parse_setting {
    ($($ty:ty),* $(,)?) => {
        $(
            impl ParseSetting for $ty {
                fn parse_setting(s: &str) -> Result<Self, String> {
                    s.parse::<$ty>().map_err(|e| e.to_string())
                }
            }
        )*
    };
}

impl_parse_setting!(u32, i32, u64, i64);

/// A byte count written as a size (`20G`, `512M`, `1024`).
///
/// Its own type rather than `u64`, whose plain number parse would read `20G`
/// as garbage and quietly fall back to the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ParseSetting for ByteSize {
    fn parse_setting(s: &str) -> Result<Self, String> {
        aurcache_common::units::parse_size(s)
            .map(ByteSize)
            .ok_or_else(|| format!("expected a size such as 20G, got {s:?}"))
    }
}

impl ParseSetting for String {
    fn parse_setting(s: &str) -> Result<Self, String> {
        Ok(s.to_string())
    }
}

impl ParseSetting for bool {
    /// Accepts the spellings that turn up in environment variables, not just
    /// Rust's `true`/`false` — `BUILD_ON_NEW_VERSION=1` should work.
    fn parse_setting(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" | "" => Ok(false),
            other => Err(format!("expected a boolean, got {other:?}")),
        }
    }
}

impl<T> ParseSetting for Option<T>
where
    T: FromStr + Clone,
    <T as FromStr>::Err: std::fmt::Display,
{
    fn parse_setting(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            Ok(None)
        } else {
            s.parse::<T>().map(Some).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ParseSetting;

    /// Settings can come from environment variables, where `1` and `yes` are
    /// as idiomatic as `true`. Rejecting them would silently disable a feature
    /// someone believes they turned on.
    #[test]
    fn booleans_accept_the_spellings_people_actually_write() {
        for on in ["true", "TRUE", "1", "yes", "on", " true "] {
            assert_eq!(bool::parse_setting(on), Ok(true), "{on:?}");
        }
        for off in ["false", "0", "no", "off", ""] {
            assert_eq!(bool::parse_setting(off), Ok(false), "{off:?}");
        }
    }

    /// A typo must not quietly read as `false` — that is the failure mode
    /// where someone thinks a feature is on and it never runs.
    #[test]
    fn an_unrecognised_boolean_is_an_error() {
        assert!(bool::parse_setting("ture").is_err());
        assert!(bool::parse_setting("enabled").is_err());
    }
}
