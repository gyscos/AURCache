//! The catalogue: every kind of structured event this build can record.
//!
//! One enum, one variant per kind, and that variant *is* the declaration --
//! there is no separate struct somewhere else and no list mapping one to the
//! other. Adding an event is adding a variant; the two `match`es below then
//! refuse to compile until it has a severity and a sentence.
//!
//! # Why an enum
//!
//! A stored row is a `kind` and a payload. Turning those back into the thing
//! they came from -- to re-render a sentence, to translate one, to check a row
//! -- is dispatch, and done by hand it is a `match kind { .. }` somebody has to
//! remember to extend. That shape has already rotted once here: the activity
//! log's `deserialize_type` silently drops a row whose kind it does not know.
//!
//! So the enum is adjacently tagged, `#[serde(tag = "kind", content = "data")]`,
//! which is exactly how a row is stored: the tag in the `kind` column, the
//! content in `data`. Assemble the two and serde dispatches on its own, with
//! nothing to keep in step. Adjacent rather than internal tagging because
//! internal tagging cannot be combined with `deny_unknown_fields`, and because
//! it is the representation the storage already has.
//!
//! A registry crate (`typetag`, `inventory`) would let variants live beside
//! their subsystems instead, at the cost of a dependency and life-before-main
//! registration. Here that advantage does not exist: the browser renders these
//! as HTML, so they have to live in this crate -- the one the wasm frontend can
//! reach -- wherever they are declared. And a registry whose entries an
//! optimiser strips in the wasm build fails by silently rendering every row as
//! its fallback sentence, which looks exactly like "that renderer is not
//! written yet".
//!
//! Lives here rather than in `aurcache-activitylog` for the same reason: that
//! crate reaches the database and can never compile to wasm. Rendering to the
//! stored columns, decoding them back, and the write queue stay there.
//!
//! See `design/structured-logs.md`.

use crate::api::activity::Severity;
use crate::api::log::{BuildRef, PackageRef};
use serde::{Deserialize, Serialize};

/// Anything worth recording, tagged by its kind.
///
/// Serializes to `{"kind": .., "data": {..}}` -- the two columns a row is
/// stored in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum Event {
    // -----------------------------------------------------------------------
    // Resolving a package's source
    // -----------------------------------------------------------------------
    /// The source checkout could not be brought up to date, so this package's
    /// source is as stale as it was.
    ///
    /// One kind for the git remote and the snapshot cache alike; `target` says
    /// which, because that is a detail worth keeping and not worth a kind of
    /// its own.
    #[serde(rename = "source.refresh_failed")]
    SourceRefreshFailed {
        pkg: PackageRef,
        target: RefreshTarget,
        error: String,
    },

    /// The `.SRCINFO` could not be produced from the checkout.
    ///
    /// A patch that no longer applies is the usual cause, and the build will
    /// fail the same way later -- which is why one package's failure does not
    /// abort the pass. `purpose` says which of the package's tracking stops
    /// being updated.
    #[serde(rename = "source.sourceinfo_failed")]
    SourceinfoFailed {
        pkg: PackageRef,
        purpose: SourceinfoPurpose,
        error: String,
    },

    /// The `git+` sources a package names could not be resolved, so a moved
    /// upstream will not be noticed until a later round succeeds.
    #[serde(rename = "vcs.sync_failed")]
    VcsSyncFailed { pkg: PackageRef, error: String },

    // -----------------------------------------------------------------------
    // Deciding what is out of date
    // -----------------------------------------------------------------------
    /// The AUR no longer lists this package.
    ///
    /// Its metadata still comes from the checkout, which continues to exist, so
    /// this is worth saying rather than leaving to be inferred from a page that
    /// stopped changing.
    #[serde(rename = "version_check.aur_missing")]
    AurMissing { pkg: PackageRef },

    /// A check's result could not be written back, so the package looks
    /// unchecked and the next pass does the work again.
    #[serde(rename = "version_check.store_failed")]
    VersionCheckStoreFailed { pkg: PackageRef, error: String },

    /// Two versions that could not be compared, so the check fell back to
    /// asking only whether they differ.
    ///
    /// The pair is the point: fused into a sentence they were something to
    /// read, and apart they are something to filter on.
    #[serde(rename = "version.compare_fallback")]
    VersionCompareFallback {
        pkg: PackageRef,
        upstream_version: String,
        built_version: String,
    },

    /// Packages were found to be out of date and none of them was queued.
    ///
    /// The check worked and the builds did not happen, which looks exactly like
    /// nothing being out of date unless it says so.
    #[serde(rename = "update.queue_failed")]
    UpdateQueueFailed { error: String },

    // -----------------------------------------------------------------------
    // Publishing a finished build
    // -----------------------------------------------------------------------
    /// A build finished and its dependents were not triggered, so packages that
    /// were waiting on it are still waiting.
    #[serde(rename = "dependents.trigger_failed")]
    DependentsTriggerFailed { pkg: PackageRef, error: String },

    /// A build could not be marked failed after its publish failed, so the row
    /// says something other than what happened.
    #[serde(rename = "build.mark_failed")]
    BuildMarkFailed { build: BuildRef, error: String },

    /// A line could not be appended to a build's log.
    ///
    /// The build is unaffected; what is lost is the explanation in the place
    /// somebody would look for it.
    #[serde(rename = "build_log.append_failed")]
    BuildLogAppendFailed { build: BuildRef, error: String },
}

/// Which store a refresh was against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTarget {
    /// The persistent checkout of a `git+` source.
    Git,
    /// The rendered snapshot of an AUR package's sources.
    Snapshot,
}

impl RefreshTarget {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Snapshot => "snapshot",
        }
    }
}

/// What a `.SRCINFO` was being read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceinfoPurpose {
    /// The version the recipe declares.
    Version,
    /// The `git+` sources, to see whether any has moved.
    Vcs,
}

impl SourceinfoPurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Vcs => "vcs",
        }
    }
}

impl Event {
    /// This event's stable identity, as the `kind` column holds it.
    ///
    /// The same string serde tags it with -- asserted below, since the two are
    /// written separately.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SourceRefreshFailed { .. } => "source.refresh_failed",
            Self::SourceinfoFailed { .. } => "source.sourceinfo_failed",
            Self::VcsSyncFailed { .. } => "vcs.sync_failed",
            Self::AurMissing { .. } => "version_check.aur_missing",
            Self::VersionCheckStoreFailed { .. } => "version_check.store_failed",
            Self::VersionCompareFallback { .. } => "version.compare_fallback",
            Self::UpdateQueueFailed { .. } => "update.queue_failed",
            Self::DependentsTriggerFailed { .. } => "dependents.trigger_failed",
            Self::BuildMarkFailed { .. } => "build.mark_failed",
            Self::BuildLogAppendFailed { .. } => "build_log.append_failed",
        }
    }

    /// How much attention it deserves, from the kind rather than the caller.
    ///
    /// Exhaustive with no default arm, so a new event does not compile until it
    /// has chosen -- "publishing failed" and "package added" are not one event
    /// with a field.
    #[must_use]
    pub const fn severity(&self) -> Severity {
        match self {
            Self::SourceRefreshFailed { .. }
            | Self::SourceinfoFailed { .. }
            | Self::VcsSyncFailed { .. }
            | Self::AurMissing { .. }
            | Self::VersionCheckStoreFailed { .. }
            | Self::VersionCompareFallback { .. }
            | Self::BuildLogAppendFailed { .. } => Severity::Warning,
            Self::UpdateQueueFailed { .. }
            | Self::DependentsTriggerFailed { .. }
            | Self::BuildMarkFailed { .. } => Severity::Error,
        }
    }

    /// The sentence, rendered now and stored, so a row still reads when its
    /// payload can no longer be parsed into this enum.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::SourceRefreshFailed { pkg, target, error } => format!(
                "could not refresh the {} source of {pkg}: {error}",
                target.as_str()
            ),
            Self::SourceinfoFailed {
                pkg,
                purpose,
                error,
            } => format!(
                "could not read the sourceinfo of {pkg} for its {}: {error}",
                purpose.as_str()
            ),
            Self::VcsSyncFailed { pkg, error } => {
                format!("could not sync the VCS sources of {pkg}: {error}")
            }
            Self::AurMissing { pkg } => format!("{pkg} is no longer in the AUR"),
            Self::VersionCheckStoreFailed { pkg, error } => {
                format!("could not store the version check result for {pkg}: {error}")
            }
            Self::VersionCompareFallback {
                pkg,
                upstream_version,
                built_version,
            } => format!(
                "cannot compare versions for {pkg}: upstream {upstream_version:?} vs built \
                 {built_version:?}"
            ),
            Self::UpdateQueueFailed { error } => {
                format!("found packages out of date but could not queue their builds: {error}")
            }
            Self::DependentsTriggerFailed { pkg, error } => {
                format!("could not trigger what depends on {pkg}: {error}")
            }
            Self::BuildMarkFailed { build, error } => {
                format!("could not mark {build} failed: {error}")
            }
            Self::BuildLogAppendFailed { build, error } => {
                format!("could not write to the log of {build}: {error}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, so the checks below cover the catalogue rather than a
    /// sample of it. A new event does not compile until it is listed.
    fn one_of_each() -> Vec<Event> {
        let pkg = || PackageRef::from("hello");
        let build = || BuildRef {
            pkgbase: "hello".to_string(),
            number: 7,
        };
        let error = || "boom".to_string();
        vec![
            Event::SourceRefreshFailed {
                pkg: pkg(),
                target: RefreshTarget::Git,
                error: error(),
            },
            Event::SourceinfoFailed {
                pkg: pkg(),
                purpose: SourceinfoPurpose::Vcs,
                error: error(),
            },
            Event::VcsSyncFailed {
                pkg: pkg(),
                error: error(),
            },
            Event::AurMissing { pkg: pkg() },
            Event::VersionCheckStoreFailed {
                pkg: pkg(),
                error: error(),
            },
            Event::VersionCompareFallback {
                pkg: pkg(),
                upstream_version: "2.0".to_string(),
                built_version: "1.0".to_string(),
            },
            Event::UpdateQueueFailed { error: error() },
            Event::DependentsTriggerFailed {
                pkg: pkg(),
                error: error(),
            },
            Event::BuildMarkFailed {
                build: build(),
                error: error(),
            },
            Event::BuildLogAppendFailed {
                build: build(),
                error: error(),
            },
        ]
    }

    /// The kind an event reports and the tag serde writes are two separate
    /// pieces of code, so this is the assertion that keeps them one string.
    #[test]
    fn the_reported_kind_is_the_tag_it_serializes_with() {
        for event in one_of_each() {
            let wire = serde_json::to_value(&event).unwrap();
            assert_eq!(wire["kind"], event.kind(), "{event:?}");
        }
    }

    /// Two events sharing a kind would be one filter returning both, and a row
    /// that decodes to whichever serde reached first.
    #[test]
    fn every_kind_is_unique() {
        let mut kinds: Vec<_> = one_of_each().iter().map(Event::kind).collect();
        let total = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), total, "a kind is declared twice");
    }

    /// `domain.verb_object`, lowercase: the shape a UI catalogue and a
    /// `kind LIKE 'worker.%'` family filter both rely on.
    #[test]
    fn every_kind_is_a_dotted_lowercase_name() {
        for event in one_of_each() {
            let kind = event.kind();
            assert!(kind.contains('.'), "{kind} has no domain");
            assert!(
                kind.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{kind} should be lowercase with `.` and `_` only"
            );
            assert!(!kind.starts_with('.') && !kind.ends_with('.'), "{kind}");
        }
    }

    /// Every event says something, and says it about what it names.
    #[test]
    fn every_event_renders_a_sentence() {
        for event in one_of_each() {
            let message = event.message();
            assert!(!message.trim().is_empty(), "{event:?} renders nothing");
            assert!(
                !message.contains("{"),
                "{message} looks like an unsubstituted template"
            );
        }
    }

    /// The round trip the stored columns rely on: out to `{kind, data}`, back
    /// to the same event.
    #[test]
    fn every_event_round_trips_through_its_wire_form() {
        for event in one_of_each() {
            let wire = serde_json::to_value(&event).unwrap();
            let back: Event = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                wire,
                "{event:?} did not survive"
            );
        }
    }
}
