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
use crate::api::log::{BuildRef, EntityRef, PackageRef, WorkerRef};
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

    /// A build produced a package that never reached the repository.
    ///
    /// Distinct from a build that failed: this one worked, and the artifact
    /// exists.
    #[serde(rename = "publish.failed")]
    PublishFailed { build: BuildRef, error: String },

    // -----------------------------------------------------------------------
    // What people did to packages
    // -----------------------------------------------------------------------
    /// A package was added, with whatever it depends on.
    #[serde(rename = "package.added")]
    PackageAdded { pkg: PackageRef },

    /// A package's build was queued on request, or by the auto-updater.
    ///
    /// `forced` rebuilds what is already current, which is worth telling apart
    /// from an update that found something new.
    #[serde(rename = "package.updated")]
    PackageUpdated { pkg: PackageRef, forced: bool },

    /// A package was removed, with its builds and artifacts.
    #[serde(rename = "package.deleted")]
    PackageDeleted { pkg: PackageRef },

    // -----------------------------------------------------------------------
    // The server and its fleet
    // -----------------------------------------------------------------------
    /// The server process started: a deploy, a restart, or a crash loop -- all
    /// three worth lining up against what else happened.
    ///
    /// The marker "since the last restart" counts back to.
    #[serde(rename = "server.start")]
    ServerStarted { version: String },

    /// A pass of the version check did not finish, so nothing was found to be
    /// out of date that pass.
    #[serde(rename = "version_check.pass_failed")]
    VersionCheckFailed { error: String },

    /// A machine asked to join the fleet for the first time.
    ///
    /// Only the first time: a worker registers on every startup, so recording
    /// each one would turn an ordinary restart into a log nobody can read past.
    #[serde(rename = "worker.enrolled")]
    WorkerEnrolled { worker: WorkerRef },

    /// A worker was let into the fleet, by someone or by the enrollment rules.
    #[serde(rename = "worker.approved")]
    WorkerApproved { worker: WorkerRef },

    /// A worker was put out of the fleet, and its builds taken back.
    #[serde(rename = "worker.revoked")]
    WorkerRevoked { worker: WorkerRef },

    /// Builds taken back from a worker that stopped answering.
    ///
    /// One entry per pass rather than per build: the reaper finds them
    /// together, and they have one cause.
    #[serde(rename = "worker.reaped")]
    WorkerReaped {
        /// Handed back to the queue for another attempt.
        retried: Vec<BuildRef>,
        /// Out of attempts, and failed outright.
        failed: Vec<BuildRef>,
    },

    /// A worker refused values its machine was configured with, and is running
    /// something else.
    #[serde(rename = "worker.setting_rejected")]
    WorkerSettingRejected {
        worker: WorkerRef,
        settings: Vec<String>,
    },
}

/// A piece of an event's sentence: prose, or something it names.
///
/// Kept apart so one sentence serves both readers: [`Event::message`] joins
/// the pieces into the text that is stored, and the browser renders each
/// reference as a link to its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Entity(EntityRef),
}

fn text(text: impl Into<String>) -> Segment {
    Segment::Text(text.into())
}

fn entity(entity: &(impl Clone + Into<EntityRef>)) -> Segment {
    Segment::Entity(entity.clone().into())
}

/// `a`, `a and b`, `a, b and c`: a list reads as a sentence, and each item
/// still links.
fn list<T: Clone + Into<EntityRef>>(items: &[T]) -> Vec<Segment> {
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(text(if index + 1 == items.len() {
                " and "
            } else {
                ", "
            }));
        }
        out.push(entity(item));
    }
    out
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
            Self::PublishFailed { .. } => "publish.failed",
            Self::PackageAdded { .. } => "package.added",
            Self::PackageUpdated { .. } => "package.updated",
            Self::PackageDeleted { .. } => "package.deleted",
            Self::ServerStarted { .. } => SERVER_START,
            Self::VersionCheckFailed { .. } => "version_check.pass_failed",
            Self::WorkerEnrolled { .. } => "worker.enrolled",
            Self::WorkerApproved { .. } => "worker.approved",
            Self::WorkerRevoked { .. } => "worker.revoked",
            Self::WorkerReaped { .. } => "worker.reaped",
            Self::WorkerSettingRejected { .. } => "worker.setting_rejected",
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
            Self::PackageAdded { .. }
            | Self::PackageUpdated { .. }
            | Self::PackageDeleted { .. }
            | Self::ServerStarted { .. }
            | Self::WorkerEnrolled { .. }
            | Self::WorkerApproved { .. }
            | Self::WorkerRevoked { .. } => Severity::Info,
            Self::SourceRefreshFailed { .. }
            | Self::SourceinfoFailed { .. }
            | Self::VcsSyncFailed { .. }
            | Self::AurMissing { .. }
            | Self::VersionCheckStoreFailed { .. }
            | Self::VersionCompareFallback { .. }
            | Self::BuildLogAppendFailed { .. }
            | Self::VersionCheckFailed { .. }
            | Self::WorkerReaped { .. }
            | Self::WorkerSettingRejected { .. } => Severity::Warning,
            Self::UpdateQueueFailed { .. }
            | Self::DependentsTriggerFailed { .. }
            | Self::BuildMarkFailed { .. }
            | Self::PublishFailed { .. } => Severity::Error,
        }
    }

    /// The sentence, as prose and the things it names.
    #[must_use]
    pub fn sentence(&self) -> Vec<Segment> {
        match self {
            Self::SourceRefreshFailed { pkg, target, error } => vec![
                text(format!(
                    "could not refresh the {} source of ",
                    target.as_str()
                )),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::SourceinfoFailed {
                pkg,
                purpose,
                error,
            } => vec![
                text("could not read the sourceinfo of "),
                entity(pkg),
                text(format!(" for its {}: {error}", purpose.as_str())),
            ],
            Self::VcsSyncFailed { pkg, error } => vec![
                text("could not sync the VCS sources of "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::AurMissing { pkg } => vec![entity(pkg), text(" is no longer in the AUR")],
            Self::VersionCheckStoreFailed { pkg, error } => vec![
                text("could not store the version check result for "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::VersionCompareFallback {
                pkg,
                upstream_version,
                built_version,
            } => vec![
                text("cannot compare versions for "),
                entity(pkg),
                text(format!(
                    ": upstream {upstream_version:?} vs built {built_version:?}"
                )),
            ],
            Self::UpdateQueueFailed { error } => vec![text(format!(
                "found packages out of date but could not queue their builds: {error}"
            ))],
            Self::DependentsTriggerFailed { pkg, error } => vec![
                text("could not trigger what depends on "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::BuildMarkFailed { build, error } => vec![
                text("could not mark "),
                entity(build),
                text(format!(" failed: {error}")),
            ],
            Self::BuildLogAppendFailed { build, error } => vec![
                text("could not write to the log of "),
                entity(build),
                text(format!(": {error}")),
            ],
            Self::PublishFailed { build, error } => vec![
                text("publishing "),
                entity(build),
                text(format!(" failed: {error}")),
            ],
            Self::PackageAdded { pkg } => vec![text("added package "), entity(pkg)],
            Self::PackageUpdated { pkg, forced } => vec![
                text(if *forced {
                    "forced update of package "
                } else {
                    "updated package "
                }),
                entity(pkg),
            ],
            Self::PackageDeleted { pkg } => vec![text("deleted package "), entity(pkg)],
            Self::ServerStarted { version } => vec![text(format!("AURCache {version} started"))],
            Self::VersionCheckFailed { error } => {
                vec![text(format!("the version check did not finish: {error}"))]
            }
            Self::WorkerEnrolled { worker } => {
                vec![text("worker "), entity(worker), text(" enrolled")]
            }
            Self::WorkerApproved { worker } => vec![text("approved worker "), entity(worker)],
            Self::WorkerRevoked { worker } => vec![text("revoked worker "), entity(worker)],
            Self::WorkerReaped { retried, failed } => {
                let mut out = vec![text("a worker stopped answering: ")];
                if !retried.is_empty() {
                    out.push(text("requeued "));
                    out.extend(list(retried));
                }
                if !failed.is_empty() {
                    if !retried.is_empty() {
                        out.push(text(", "));
                    }
                    out.push(text("gave up on "));
                    out.extend(list(failed));
                }
                out
            }
            Self::WorkerSettingRejected { worker, settings } => vec![
                text("worker "),
                entity(worker),
                text(format!(
                    " refused {} it was configured with: {}",
                    if settings.len() == 1 {
                        "a value"
                    } else {
                        "values"
                    },
                    settings.join(", ")
                )),
            ],
        }
    }

    /// The sentence as text, rendered now and stored, so a row still reads
    /// when its payload can no longer be parsed into this enum.
    #[must_use]
    pub fn message(&self) -> String {
        self.sentence()
            .into_iter()
            .map(|segment| match segment {
                Segment::Text(text) => text,
                Segment::Entity(entity) => entity.label(),
            })
            .collect()
    }

    /// Rebuild an event from the two columns a row stores it in.
    ///
    /// `None` for a kind this build does not know -- a row from a newer server
    /// -- or a payload that no longer fits its variant: the cases the stored
    /// message exists for.
    #[must_use]
    pub fn decode(kind: &str, data: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(serde_json::json!({ "kind": kind, "data": data })).ok()
    }
}

/// The kind of [`Event::ServerStarted`]: the boot marker the server itself has
/// to recognise, for "since the last restart".
pub const SERVER_START: &str = "server.start";

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
        let worker = || WorkerRef::from("builder-01");
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
            Event::PublishFailed {
                build: build(),
                error: error(),
            },
            Event::PackageAdded { pkg: pkg() },
            Event::PackageUpdated {
                pkg: pkg(),
                forced: true,
            },
            Event::PackageDeleted { pkg: pkg() },
            Event::ServerStarted {
                version: "1.2.3".to_string(),
            },
            Event::VersionCheckFailed { error: error() },
            Event::WorkerEnrolled { worker: worker() },
            Event::WorkerApproved { worker: worker() },
            Event::WorkerRevoked { worker: worker() },
            Event::WorkerReaped {
                retried: vec![build()],
                failed: vec![build(), build()],
            },
            Event::WorkerSettingRejected {
                worker: worker(),
                settings: vec!["builddir_max_bytes".to_string()],
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

    /// The stored sentence names things by what people call them, not by
    /// their namespaced ids.
    #[test]
    fn a_message_names_entities_by_their_labels() {
        let message = Event::PublishFailed {
            build: BuildRef {
                pkgbase: "hello".to_string(),
                number: 7,
            },
            error: "disk full".to_string(),
        }
        .message();
        assert_eq!(message, "publishing hello #7 failed: disk full");
    }

    /// Every entity a payload names appears in its sentence, so the browser can
    /// link each one and none goes unmentioned.
    #[test]
    fn every_reference_in_a_payload_is_in_its_sentence() {
        for event in one_of_each() {
            let named: Vec<EntityRef> = event
                .sentence()
                .into_iter()
                .filter_map(|segment| match segment {
                    Segment::Entity(entity) => Some(entity),
                    Segment::Text(_) => None,
                })
                .collect();
            let wire = serde_json::to_value(&event).unwrap();
            for value in wire["data"].as_object().unwrap().values() {
                let items = match value {
                    serde_json::Value::Array(items) => items.clone(),
                    other => vec![other.clone()],
                };
                for item in items {
                    if let Some(entity) =
                        item.as_str().and_then(|raw| raw.parse::<EntityRef>().ok())
                    {
                        assert!(
                            named.contains(&entity),
                            "{event:?} does not mention {entity:?}"
                        );
                    }
                }
            }
        }
    }

    /// Lists read as a sentence: `a`, `a and b`, `a, b and c`.
    #[test]
    fn a_reaped_worker_lists_its_builds_as_a_sentence() {
        let build = |number| BuildRef {
            pkgbase: "hello".to_string(),
            number,
        };
        let message = Event::WorkerReaped {
            retried: vec![build(7)],
            failed: vec![build(8), build(9), build(10)],
        }
        .message();
        assert_eq!(
            message,
            "a worker stopped answering: requeued hello #7, gave up on hello #8, hello #9 and hello #10"
        );
    }

    /// A stored row comes back as the event it was written from, and an unknown
    /// kind or a stale payload comes back as nothing.
    #[test]
    fn decoding_rebuilds_known_rows_and_refuses_the_rest() {
        let event = Event::PackageAdded {
            pkg: "hello".into(),
        };
        let wire = serde_json::to_value(&event).unwrap();
        let back = Event::decode(event.kind(), &wire["data"]).expect("decodes");
        assert_eq!(back.message(), event.message());
        assert!(Event::decode("from.the.future", &serde_json::json!({})).is_none());
        assert!(Event::decode("package.added", &serde_json::json!({"nope": 1})).is_none());
    }
}
