//! What a worker says it can be configured with, and what it is actually running.
//!
//! A worker *declares* its settings at registration ([`SettingDecl`]) and
//! reports what each one resolved to ([`EffectiveConfig`]). The server stores
//! both and renders them; it does not know what `build_memory_max` or
//! `concurrency` *mean*, only their [`ValueKind`].
//!
//! That is the whole reason the declaration travels rather than living on the
//! server: the server and its workers are upgraded separately, and different
//! worker implementations have different settings. A server-side list would
//! need a server release before a new worker setting could be seen at all.
//!
//! See `design/worker-configuration.md`.

use crate::units::{parse_duration, parse_size};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// The type of a declared setting: the one thing the server and a worker must
/// agree on, since it is what the server validates and renders against.
///
/// Deliberately small and closed. Everything else about a setting — what it
/// does, what it defaults to, when it takes effect — travels as data in the
/// declaration, so adding a worker setting never needs a change here.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ValueKind {
    /// A whole number, optionally bounded. Both bounds are inclusive.
    Integer {
        #[serde(default)]
        min: Option<i64>,
        #[serde(default)]
        max: Option<i64>,
    },
    /// A fractional number, optionally bounded. Both bounds are inclusive.
    Float {
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
    },
    /// `true` or `false`.
    Bool,
    /// A byte count written the way configuration writes one (`450G`), read by
    /// [`parse_size`].
    Size,
    /// A span written the way configuration writes one (`3h`, `1h30m`), read by
    /// [`parse_duration`] into seconds.
    Duration,
    /// One of a fixed set of words.
    Choice { options: Vec<String> },
    /// Free text.
    Text,
    /// Comma-separated items.
    List,
    /// A kind this build does not know, from a worker newer than the server.
    ///
    /// Kept as a variant rather than a deserialization failure: one unrecognised
    /// setting must not cost the server the whole declaration. It renders as
    /// text, and the worker validates its own values anyway.
    #[serde(other)]
    Unknown,
}

impl ValueKind {
    /// Check a written value against this kind, returning why it does not fit.
    ///
    /// The message is for an operator reading it beside the field they typed
    /// it into, so it says what was expected rather than naming a parser.
    ///
    /// # Errors
    ///
    /// Returns the reason the value is not of this kind.
    pub fn validate(&self, raw: &str) -> Result<(), String> {
        let raw = raw.trim();
        match self {
            Self::Integer { min, max } => {
                let value: i64 = raw
                    .parse()
                    .map_err(|_| format!("{raw:?} is not a whole number"))?;
                check_bounds(value, *min, *max, raw)
            }
            Self::Float { min, max } => {
                let value: f64 = raw
                    .parse()
                    .map_err(|_| format!("{raw:?} is not a number"))?;
                if !value.is_finite() {
                    return Err(format!("{raw:?} is not a finite number"));
                }
                check_bounds(value, *min, *max, raw)
            }
            Self::Bool => match raw {
                "true" | "false" => Ok(()),
                _ => Err(format!("{raw:?} is not true or false")),
            },
            Self::Size => parse_size(raw).map(|_| ()).ok_or_else(|| {
                format!(
                    "{raw:?} is not a size (bytes, or a size such as 500M, 450G, 450GiB or 450GB)"
                )
            }),
            Self::Duration => parse_duration(raw).map(|_| ()).ok_or_else(|| {
                format!(
                    "{raw:?} is not a duration (seconds, or a span such as 90s, 15m, 3h, 1h30m \
                     or 30d)"
                )
            }),
            Self::Choice { options } => {
                if options.iter().any(|option| option == raw) {
                    Ok(())
                } else {
                    Err(format!("{raw:?} is not one of {}", options.join(", ")))
                }
            }
            // A newer worker's kind is not ours to judge; it validates on
            // delivery, where a bad value becomes a rejected status.
            Self::Text | Self::List | Self::Unknown => Ok(()),
        }
    }
}

/// Range check shared by the numeric kinds.
fn check_bounds<T>(value: T, min: Option<T>, max: Option<T>, raw: &str) -> Result<(), String>
where
    T: PartialOrd + std::fmt::Display,
{
    if let Some(min) = min
        && value < min
    {
        return Err(format!("{raw:?} is below the minimum of {min}"));
    }
    if let Some(max) = max
        && value > max
    {
        return Err(format!("{raw:?} is above the maximum of {max}"));
    }
    Ok(())
}

/// When a change to a setting takes hold on a running worker.
///
/// Reported so the UI can say so rather than leaving an operator to wonder
/// whether a save did anything yet. Nothing on the server acts on it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// Read afresh by the next build that starts; builds already running keep
    /// the value they started with.
    NextJob,
    /// Picked up by the next turn of the loop that uses it (a cache sweep, a
    /// chroot refresh).
    NextLoop,
    /// In force as soon as the worker applies the change.
    Immediately,
}

/// One setting a worker accepts.
///
/// Compiled into the worker, never derived from its environment: a worker runs
/// devtools as root, and a server that could name any `WORKER_*` variable could
/// name the ones that decide what a build may reach (`WORKER_BIND_MOUNTS`,
/// `WORKER_BUILD_USER`). The set of keys is a security boundary, so it is
/// stated in code by the side the boundary protects.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SettingDecl {
    /// Stable identifier, e.g. `build_memory_max`.
    pub key: String,
    pub kind: ValueKind,
    /// One line an operator reads to decide what to set it to.
    pub description: String,
    /// Grouping for display, e.g. `Build limits`, `Caches`.
    pub category: String,
    /// The fallback actually in effect: the `<env_var>_DEFAULT` this worker was
    /// started with, if it has one, else the built-in default. `None` where
    /// unset means unlimited.
    ///
    /// The *real* fallback rather than the built-in one, so resetting a value
    /// shows what the worker would return to.
    #[serde(default)]
    pub default: Option<String>,
    /// The environment variable that pins this setting on the worker's own
    /// machine. `<env_var>_DEFAULT` sets a default the server may override
    /// instead.
    #[serde(default)]
    pub env_var: Option<String>,
    pub applies: Applies,
}

/// Where the value a worker is running came from.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveSource {
    /// Pinned by the plain environment variable on the worker's machine.
    Env,
    /// Set for this worker on the server.
    Server,
    /// The `<env_var>_DEFAULT` the worker was started with.
    EnvDefault,
    /// The worker's built-in default.
    Default,
}

/// What became of a setting on the worker.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingStatus {
    /// In use.
    Applied,
    /// A server value exists but the machine's own variable pins it.
    Overridden,
    /// The worker does not declare this key (it was renamed or removed).
    Unsupported,
    /// The value could not be used, and the previous usable one stands.
    Rejected,
}

/// One setting as the worker is actually running it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct EffectiveSetting {
    /// The value in use, written the way it was configured. `None` where the
    /// setting is unset and unset means unlimited.
    #[serde(default)]
    pub value: Option<String>,
    pub source: EffectiveSource,
    pub status: SettingStatus,
    /// Why, for anything an operator would otherwise have to guess at: a value
    /// that was refused, or a pin shadowing something.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Everything a worker is running, keyed by setting.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct EffectiveConfig {
    /// The snapshot revision this reflects.
    ///
    /// `None` until the server delivers snapshots: a worker that has been sent
    /// nothing is running its environment and its defaults, which is a complete
    /// answer and not a missing one.
    #[serde(default)]
    pub received_revision: Option<String>,
    pub settings: BTreeMap<String, EffectiveSetting>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_range_checked() {
        let kind = ValueKind::Integer {
            min: Some(1),
            max: Some(8),
        };
        assert!(kind.validate("4").is_ok());
        assert!(kind.validate(" 4 ").is_ok());
        assert!(kind.validate("0").is_err());
        assert!(kind.validate("9").is_err());
        assert!(kind.validate("4.5").is_err());
        assert!(kind.validate("many").is_err());
    }

    #[test]
    fn sizes_and_durations_read_as_configuration_writes_them() {
        assert!(ValueKind::Size.validate("450G").is_ok());
        assert!(ValueKind::Size.validate("450").is_ok());
        assert!(ValueKind::Size.validate("450 giraffes").is_err());
        assert!(ValueKind::Duration.validate("1h30m").is_ok());
        assert!(ValueKind::Duration.validate("900").is_ok());
        assert!(ValueKind::Duration.validate("soon").is_err());
    }

    #[test]
    fn choices_are_exact() {
        let kind = ValueKind::Choice {
            options: vec!["auto".to_string(), "overlay".to_string()],
        };
        assert!(kind.validate("overlay").is_ok());
        assert!(kind.validate("Overlay").is_err());
    }

    /// The reason is what an operator reads, so it has to name the value they
    /// typed rather than only the expectation.
    #[test]
    fn a_refusal_quotes_the_value() {
        let reason = ValueKind::Size.validate("450 giraffes").unwrap_err();
        assert!(reason.contains("450 giraffes"), "{reason}");
    }

    /// A kind from a worker newer than this build must cost the declaration
    /// nothing: it becomes [`ValueKind::Unknown`] and the rest still reads.
    #[test]
    fn an_unknown_kind_does_not_fail_the_declaration() {
        let json = r#"{
            "key": "future_thing",
            "kind": {"type": "colour", "space": "srgb"},
            "description": "d",
            "category": "c",
            "applies": "next_job"
        }"#;
        let decl: SettingDecl = serde_json::from_str(json).unwrap();
        assert_eq!(decl.kind, ValueKind::Unknown);
        assert_eq!(decl.key, "future_thing");
        assert!(decl.kind.validate("anything").is_ok());
    }

    #[test]
    fn effective_config_round_trips() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "concurrency".to_string(),
            EffectiveSetting {
                value: Some("2".to_string()),
                source: EffectiveSource::Env,
                status: SettingStatus::Applied,
                reason: None,
            },
        );
        let config = EffectiveConfig {
            received_revision: None,
            settings,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(
            serde_json::from_str::<EffectiveConfig>(&json).unwrap(),
            config
        );
    }
}
