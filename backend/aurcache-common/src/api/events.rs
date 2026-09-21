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
    WorkerRevoked {
        worker: WorkerRef,
        /// The builds it was running, handed back to the queue. Absent from
        /// entries written before it was recorded.
        #[serde(default)]
        requeued: Vec<BuildRef>,
    },

    /// Builds taken back from a worker that stopped answering.
    ///
    /// One entry per pass rather than per build: the reaper finds them
    /// together, and they have one cause.
    #[serde(rename = "worker.reaped")]
    WorkerReaped {
        /// The workers that stopped answering. Absent from entries written
        /// before it was recorded.
        #[serde(default)]
        workers: Vec<WorkerRef>,
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

    /// A worker checked in at startup. Every start, not only the first: the
    /// version it reports is how a fleet upgrade shows up.
    #[serde(rename = "worker.registered")]
    WorkerRegistered {
        worker: WorkerRef,
        #[serde(default)]
        version: Option<String>,
    },

    /// What a worker's settings resolved to changed since it last reported.
    #[serde(rename = "worker.config_changed")]
    WorkerConfigChanged {
        worker: WorkerRef,
        settings: Vec<String>,
    },

    /// Something a worker sent about itself could not be stored or read, so
    /// its page shows less than it reported.
    #[serde(rename = "worker.report_failed")]
    WorkerReportFailed {
        worker: WorkerRef,
        /// Which report: its setting declaration, or what they resolved to.
        report: WorkerReport,
        error: String,
    },

    // -----------------------------------------------------------------------
    // Builds
    // -----------------------------------------------------------------------
    /// A worker took a build.
    #[serde(rename = "build.started")]
    BuildStarted { build: BuildRef, worker: WorkerRef },

    /// A worker finished a build, and it is on its way into the repository.
    #[serde(rename = "build.succeeded")]
    BuildSucceeded { build: BuildRef, worker: WorkerRef },

    /// A build failed on its worker.
    #[serde(rename = "build.failed")]
    BuildFailed {
        build: BuildRef,
        worker: WorkerRef,
        /// What the worker said, when it said anything.
        #[serde(default)]
        reason: Option<String>,
    },

    /// A finished build reached the repository.
    #[serde(rename = "build.published")]
    BuildPublished { build: BuildRef },

    /// A worker reported a build finished, and the report was refused -- which
    /// is how a build that worked becomes one that failed.
    #[serde(rename = "build.completion_rejected")]
    BuildCompletionRejected {
        build: BuildRef,
        worker: WorkerRef,
        reason: String,
    },

    /// Something a worker reported about a build could not be written down,
    /// so the build page shows less than happened.
    ///
    /// One kind for every such fact; `what` says which, since nobody filters
    /// peak memory apart from failure reasons.
    #[serde(rename = "build.record_failed")]
    BuildRecordFailed {
        build: BuildRef,
        what: String,
        error: String,
    },

    /// Somebody stopped a build.
    #[serde(rename = "build.cancelled")]
    BuildCancelled { build: BuildRef },

    /// Somebody queued a failed build again.
    #[serde(rename = "build.retried")]
    BuildRetried { build: BuildRef },

    /// Somebody deleted a build and its log.
    #[serde(rename = "build.deleted")]
    BuildDeleted { build: BuildRef },

    /// A package could not be queued at startup, and will not build until it
    /// is fixed.
    #[serde(rename = "build.enqueue_skipped")]
    EnqueueSkipped { pkg: PackageRef, error: String },

    /// Nothing waiting was queued at startup, so builds that should have
    /// resumed did not.
    #[serde(rename = "build.startup_enqueue_failed")]
    StartupEnqueueFailed { error: String },

    // -----------------------------------------------------------------------
    // Packages and their sources
    // -----------------------------------------------------------------------
    /// Somebody changed how a package is built: its platforms or build flags.
    #[serde(rename = "package.changed")]
    PackageChanged {
        pkg: PackageRef,
        fields: Vec<String>,
    },

    /// Somebody edited one of a package's source files, which is stored as a
    /// patch against upstream.
    #[serde(rename = "source.edited")]
    SourceEdited { pkg: PackageRef, path: String },

    /// A package's source metadata -- description, maintainer, history --
    /// could not be refreshed, so its page shows what it last knew.
    #[serde(rename = "source.metadata_failed")]
    SourceMetadataFailed { pkg: PackageRef, error: String },

    /// A deleted package's source checkout could not be removed, and is taking
    /// up space until a later sweep.
    #[serde(rename = "source.checkout_remove_failed")]
    CheckoutRemoveFailed { pkg: PackageRef, error: String },

    /// A dependency nothing provides -- not the official repositories, not a
    /// tracked package, not the AUR -- so the package builds without it and
    /// will likely fail.
    #[serde(rename = "deps.unresolved")]
    DepsUnresolved { pkg: PackageRef, dependency: String },

    /// Somebody pointed a dependency of one package at another package.
    #[serde(rename = "deps.replaced")]
    DepsReplaced {
        dependent: PackageRef,
        old: PackageRef,
        new: PackageRef,
    },

    /// Somebody dropped a dependency the official repositories provide, so it
    /// is installed from there rather than built here.
    #[serde(rename = "deps.dropped")]
    DepsDropped {
        dependent: PackageRef,
        dependency: PackageRef,
    },

    /// An auto-update of one package failed; the rest went ahead.
    #[serde(rename = "update.skipped")]
    UpdateSkipped { pkg: PackageRef, error: String },

    /// A bulk add could not resolve its names in one request, and fell back
    /// to one request per package -- slower, and nothing more.
    #[serde(rename = "bulk_add.resolve_failed")]
    BulkAddResolveFailed { error: String },

    /// A bulk add or a restore stopped before its end.
    #[serde(rename = "operation.aborted")]
    OperationAborted {
        /// `bulk_add` or `restore`.
        operation: String,
        error: String,
    },

    // -----------------------------------------------------------------------
    // Settings, access and backups
    // -----------------------------------------------------------------------
    /// Somebody changed a setting -- server-wide, or for one package. The
    /// value is left out: config files are long, and some settings are
    /// nobody's business but the instance's.
    #[serde(rename = "setting.changed")]
    SettingChanged {
        key: String,
        #[serde(default)]
        pkg: Option<PackageRef>,
    },

    /// Somebody put a setting back to what it inherits.
    #[serde(rename = "setting.reset")]
    SettingReset {
        key: String,
        #[serde(default)]
        pkg: Option<PackageRef>,
    },

    /// A cron schedule could not be used, so that job is not running on it.
    #[serde(rename = "schedule.invalid")]
    ScheduleInvalid {
        /// Which job: `auto_update` or `mirror_ranking`.
        job: String,
        error: String,
    },

    /// Somebody signed in with an account that is not allowed.
    #[serde(rename = "auth.sign_in_refused")]
    SignInRefused { user: String },

    /// Somebody replaced the API token, and every client using the old one
    /// now needs the new one.
    #[serde(rename = "auth.token_regenerated")]
    TokenRegenerated {},

    /// Somebody exported a dump -- with the secrets in it, when `secrets`.
    #[serde(rename = "dump.exported")]
    DumpExported { secrets: bool },

    /// Somebody restored a dump.
    #[serde(rename = "restore.applied")]
    RestoreApplied { packages: usize },

    /// A restored package came in, but one of the steps after it did not.
    #[serde(rename = "restore.package_failed")]
    RestorePackageFailed {
        pkg: PackageRef,
        step: RestoreStep,
        error: String,
    },

    /// A restore replaced the worker CA: every certificate the current workers
    /// hold is now worthless, and each has to enroll again.
    #[serde(rename = "restore.ca_replaced")]
    RestoreCaReplaced {},

    // -----------------------------------------------------------------------
    // Mirrors and the repository
    // -----------------------------------------------------------------------
    /// The mirrors could not be ranked, so builds keep using the old order.
    #[serde(rename = "mirrors.rank_failed")]
    MirrorRankFailed { error: String },

    /// The official repositories' databases could not be refreshed, so
    /// dependency resolution works from an older copy.
    #[serde(rename = "official_repos.refresh_failed")]
    OfficialReposRefreshFailed { error: String },

    /// A change to the repository did not commit, and is being tried again.
    #[serde(rename = "repo.commit_retry")]
    RepoCommitRetried { attempt: u32, error: String },

    /// A file in the repository could not be moved or removed.
    ///
    /// A move that failed is a package the repository says it has and does
    /// not serve.
    #[serde(rename = "repo.file_failed")]
    RepoFileFailed {
        path: String,
        action: FileAction,
        error: String,
    },

    /// Retired package files were removed from the repository, once no client
    /// could still be asking for them.
    #[serde(rename = "repo.swept")]
    RepoSwept { files: Vec<String> },

    /// The sweep of retired package files did not run, so they stay on disk.
    #[serde(rename = "repo.sweep_failed")]
    RepoSweepFailed { error: String },
}

/// Which report a worker sent about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerReport {
    /// The settings it accepts.
    Declaration,
    /// What each of them resolved to on that machine.
    Configuration,
}

impl WorkerReport {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Declaration => "setting declaration",
            Self::Configuration => "configuration report",
        }
    }
}

/// The step of a restore that did not complete for one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStep {
    /// Reading its source, so nothing finds it by its split names or provides.
    Source,
    /// Resolving its dependencies, so it will not build until they are.
    Dependencies,
}

impl RestoreStep {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "its source could not be read",
            Self::Dependencies => "its dependencies could not be resolved",
        }
    }
}

/// What was being done to a repository file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Move,
    Remove,
}

impl FileAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::Remove => "remove",
        }
    }
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
            Self::WorkerReportFailed { .. } => "worker.report_failed",
            Self::BuildStarted { .. } => "build.started",
            Self::BuildSucceeded { .. } => "build.succeeded",
            Self::WorkerRegistered { .. } => "worker.registered",
            Self::WorkerConfigChanged { .. } => "worker.config_changed",
            Self::BuildFailed { .. } => "build.failed",
            Self::BuildPublished { .. } => "build.published",
            Self::BuildCompletionRejected { .. } => "build.completion_rejected",
            Self::BuildRecordFailed { .. } => "build.record_failed",
            Self::BuildCancelled { .. } => "build.cancelled",
            Self::BuildRetried { .. } => "build.retried",
            Self::BuildDeleted { .. } => "build.deleted",
            Self::EnqueueSkipped { .. } => "build.enqueue_skipped",
            Self::StartupEnqueueFailed { .. } => "build.startup_enqueue_failed",
            Self::PackageChanged { .. } => "package.changed",
            Self::SourceEdited { .. } => "source.edited",
            Self::SourceMetadataFailed { .. } => "source.metadata_failed",
            Self::CheckoutRemoveFailed { .. } => "source.checkout_remove_failed",
            Self::DepsUnresolved { .. } => "deps.unresolved",
            Self::DepsReplaced { .. } => "deps.replaced",
            Self::DepsDropped { .. } => "deps.dropped",
            Self::UpdateSkipped { .. } => "update.skipped",
            Self::BulkAddResolveFailed { .. } => "bulk_add.resolve_failed",
            Self::OperationAborted { .. } => "operation.aborted",
            Self::SettingChanged { .. } => "setting.changed",
            Self::SettingReset { .. } => "setting.reset",
            Self::ScheduleInvalid { .. } => "schedule.invalid",
            Self::SignInRefused { .. } => "auth.sign_in_refused",
            Self::TokenRegenerated {} => "auth.token_regenerated",
            Self::DumpExported { .. } => "dump.exported",
            Self::RestoreApplied { .. } => "restore.applied",
            Self::RestorePackageFailed { .. } => "restore.package_failed",
            Self::RestoreCaReplaced {} => "restore.ca_replaced",
            Self::MirrorRankFailed { .. } => "mirrors.rank_failed",
            Self::OfficialReposRefreshFailed { .. } => "official_repos.refresh_failed",
            Self::RepoCommitRetried { .. } => "repo.commit_retry",
            Self::RepoFileFailed { .. } => "repo.file_failed",
            Self::RepoSwept { .. } => "repo.swept",
            Self::RepoSweepFailed { .. } => "repo.sweep_failed",
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
            | Self::WorkerRevoked { .. }
            | Self::BuildStarted { .. }
            | Self::BuildSucceeded { .. }
            | Self::WorkerRegistered { .. }
            | Self::WorkerConfigChanged { .. }
            | Self::BuildPublished { .. }
            | Self::BuildCancelled { .. }
            | Self::BuildRetried { .. }
            | Self::BuildDeleted { .. }
            | Self::PackageChanged { .. }
            | Self::SourceEdited { .. }
            | Self::DepsReplaced { .. }
            | Self::DepsDropped { .. }
            | Self::SettingChanged { .. }
            | Self::SettingReset { .. }
            | Self::TokenRegenerated {}
            | Self::DumpExported { .. }
            | Self::RestoreApplied { .. }
            | Self::RepoSwept { .. } => Severity::Info,
            Self::SourceRefreshFailed { .. }
            | Self::SourceinfoFailed { .. }
            | Self::VcsSyncFailed { .. }
            | Self::AurMissing { .. }
            | Self::VersionCheckStoreFailed { .. }
            | Self::VersionCompareFallback { .. }
            | Self::BuildLogAppendFailed { .. }
            | Self::VersionCheckFailed { .. }
            | Self::WorkerReaped { .. }
            | Self::WorkerSettingRejected { .. }
            | Self::WorkerReportFailed { .. }
            | Self::BuildFailed { .. }
            | Self::BuildCompletionRejected { .. }
            | Self::BuildRecordFailed { .. }
            | Self::EnqueueSkipped { .. }
            | Self::SourceMetadataFailed { .. }
            | Self::CheckoutRemoveFailed { .. }
            | Self::DepsUnresolved { .. }
            | Self::UpdateSkipped { .. }
            | Self::BulkAddResolveFailed { .. }
            | Self::ScheduleInvalid { .. }
            | Self::SignInRefused { .. }
            | Self::RestorePackageFailed { .. }
            | Self::RestoreCaReplaced {}
            | Self::MirrorRankFailed { .. }
            | Self::OfficialReposRefreshFailed { .. }
            | Self::RepoCommitRetried { .. }
            | Self::RepoSweepFailed { .. } => Severity::Warning,
            Self::UpdateQueueFailed { .. }
            | Self::DependentsTriggerFailed { .. }
            | Self::BuildMarkFailed { .. }
            | Self::PublishFailed { .. }
            | Self::StartupEnqueueFailed { .. }
            | Self::OperationAborted { .. }
            | Self::RepoFileFailed { .. } => Severity::Error,
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
            Self::WorkerRevoked { worker, requeued } => {
                let mut out = vec![text("revoked worker "), entity(worker)];
                if !requeued.is_empty() {
                    out.push(text(" and requeued "));
                    out.extend(list(requeued));
                }
                out
            }
            Self::WorkerReaped {
                workers,
                retried,
                failed,
            } => {
                let mut out = if workers.is_empty() {
                    vec![text("a worker stopped answering: ")]
                } else {
                    let mut out = list(workers);
                    out.push(text(" stopped answering: "));
                    out
                };
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
            Self::WorkerReportFailed {
                worker,
                report,
                error,
            } => vec![
                text(format!(
                    "could not store the {} of worker ",
                    report.as_str()
                )),
                entity(worker),
                text(format!(": {error}")),
            ],
            Self::BuildStarted { build, worker } => {
                vec![entity(worker), text(" started "), entity(build)]
            }
            Self::BuildFailed {
                build,
                worker,
                reason,
            } => {
                let mut out = vec![entity(build), text(" failed on "), entity(worker)];
                if let Some(reason) = reason {
                    out.push(text(format!(": {reason}")));
                }
                out
            }
            Self::BuildPublished { build } => vec![text("published "), entity(build)],
            Self::BuildSucceeded { build, worker } => {
                vec![entity(build), text(" finished on "), entity(worker)]
            }
            Self::WorkerRegistered { worker, version } => {
                let mut out = vec![text("worker "), entity(worker), text(" checked in")];
                if let Some(version) = version {
                    out.push(text(format!(", running {version}")));
                }
                out
            }
            Self::WorkerConfigChanged { worker, settings } => vec![
                text("the configuration of worker "),
                entity(worker),
                text(format!(" changed: {}", settings.join(", "))),
            ],
            Self::BuildCompletionRejected {
                build,
                worker,
                reason,
            } => vec![
                text("refused "),
                entity(worker),
                text("'s report that "),
                entity(build),
                text(format!(" finished: {reason}")),
            ],
            Self::BuildRecordFailed { build, what, error } => vec![
                text(format!("could not record the {what} of ")),
                entity(build),
                text(format!(": {error}")),
            ],
            Self::BuildCancelled { build } => vec![text("stopped "), entity(build)],
            Self::BuildRetried { build } => vec![text("retried "), entity(build)],
            Self::BuildDeleted { build } => vec![text("deleted "), entity(build)],
            Self::EnqueueSkipped { pkg, error } => vec![
                text("could not queue "),
                entity(pkg),
                text(format!(" at startup: {error}")),
            ],
            Self::StartupEnqueueFailed { error } => vec![text(format!(
                "could not queue the waiting builds at startup: {error}"
            ))],
            Self::PackageChanged { pkg, fields } => vec![
                text(format!("changed the {} of ", fields.join(" and "))),
                entity(pkg),
            ],
            Self::SourceEdited { pkg, path } => {
                vec![text(format!("edited {path} of ")), entity(pkg)]
            }
            Self::SourceMetadataFailed { pkg, error } => vec![
                text("could not refresh the source metadata of "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::CheckoutRemoveFailed { pkg, error } => vec![
                text("could not remove the source checkout of "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::DepsUnresolved { pkg, dependency } => vec![
                text(format!("nothing provides {dependency}, which ")),
                entity(pkg),
                text(" depends on"),
            ],
            Self::DepsReplaced {
                dependent,
                old,
                new,
            } => vec![
                text("replaced "),
                entity(old),
                text(" with "),
                entity(new),
                text(" as a dependency of "),
                entity(dependent),
            ],
            Self::DepsDropped {
                dependent,
                dependency,
            } => vec![
                text("dropped "),
                entity(dependency),
                text(" as a dependency of "),
                entity(dependent),
                text(": the official repositories provide it"),
            ],
            Self::UpdateSkipped { pkg, error } => vec![
                text("the auto-update skipped "),
                entity(pkg),
                text(format!(": {error}")),
            ],
            Self::BulkAddResolveFailed { error } => vec![text(format!(
                "a bulk add could not resolve its names in one request, and asked one by one: \
                 {error}"
            ))],
            Self::OperationAborted { operation, error } => vec![text(format!(
                "the {} stopped before its end: {error}",
                operation.replace('_', " ")
            ))],
            Self::SettingChanged { key, pkg } => {
                let mut out = vec![text(format!("changed the setting {key}"))];
                if let Some(pkg) = pkg {
                    out.push(text(" of "));
                    out.push(entity(pkg));
                }
                out
            }
            Self::SettingReset { key, pkg } => {
                let mut out = vec![text(format!("reset the setting {key}"))];
                if let Some(pkg) = pkg {
                    out.push(text(" of "));
                    out.push(entity(pkg));
                }
                out
            }
            Self::ScheduleInvalid { job, error } => vec![text(format!(
                "the {} schedule cannot be used: {error}",
                job.replace('_', " ")
            ))],
            Self::SignInRefused { user } => {
                vec![text(format!(
                    "refused a sign-in by {user}: not an allowed user"
                ))]
            }
            Self::TokenRegenerated {} => vec![text("replaced the API token")],
            Self::DumpExported { secrets } => vec![text(if *secrets {
                "exported a dump, with the CA key and credentials in it"
            } else {
                "exported a dump"
            })],
            Self::RestoreApplied { packages } => {
                vec![text(format!("restored a dump of {packages} package(s)"))]
            }
            Self::RestorePackageFailed { pkg, step, error } => vec![
                text("restored "),
                entity(pkg),
                text(format!(", but {}: {error}", step.as_str())),
            ],
            Self::RestoreCaReplaced {} => vec![text(
                "a restore replaced the worker CA; every worker has to enroll again",
            )],
            Self::MirrorRankFailed { error } => {
                vec![text(format!("could not rank the mirrors: {error}"))]
            }
            Self::OfficialReposRefreshFailed { error } => vec![text(format!(
                "could not refresh the official repository databases: {error}"
            ))],
            Self::RepoCommitRetried { attempt, error } => vec![text(format!(
                "a repository update did not commit (attempt {attempt}), retrying: {error}"
            ))],
            Self::RepoFileFailed {
                path,
                action,
                error,
            } => vec![text(format!(
                "could not {} {path} in the repository: {error}",
                action.as_str()
            ))],
            Self::RepoSwept { files } => vec![text(format!(
                "removed {} retired file(s) from the repository: {}",
                files.len(),
                files.join(", ")
            ))],
            Self::RepoSweepFailed { error } => vec![text(format!(
                "the sweep of retired repository files failed: {error}"
            ))],
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
            Event::WorkerRevoked {
                worker: worker(),
                requeued: vec![build()],
            },
            Event::WorkerReaped {
                workers: vec![worker()],
                retried: vec![build()],
                failed: vec![build(), build()],
            },
            Event::WorkerSettingRejected {
                worker: worker(),
                settings: vec!["builddir_max_bytes".to_string()],
            },
            Event::WorkerReportFailed {
                worker: worker(),
                report: WorkerReport::Configuration,
                error: error(),
            },
            Event::BuildStarted {
                build: build(),
                worker: worker(),
            },
            Event::BuildFailed {
                build: build(),
                worker: worker(),
                reason: Some(error()),
            },
            Event::BuildPublished { build: build() },
            Event::BuildSucceeded {
                build: build(),
                worker: worker(),
            },
            Event::WorkerRegistered {
                worker: worker(),
                version: Some("1.2.3".to_string()),
            },
            Event::WorkerConfigChanged {
                worker: worker(),
                settings: vec!["concurrency".to_string()],
            },
            Event::BuildCompletionRejected {
                build: build(),
                worker: worker(),
                reason: error(),
            },
            Event::BuildRecordFailed {
                build: build(),
                what: "peak memory".to_string(),
                error: error(),
            },
            Event::BuildCancelled { build: build() },
            Event::BuildRetried { build: build() },
            Event::BuildDeleted { build: build() },
            Event::EnqueueSkipped {
                pkg: pkg(),
                error: error(),
            },
            Event::StartupEnqueueFailed { error: error() },
            Event::PackageChanged {
                pkg: pkg(),
                fields: vec!["platforms".to_string()],
            },
            Event::SourceEdited {
                pkg: pkg(),
                path: "PKGBUILD".to_string(),
            },
            Event::SourceMetadataFailed {
                pkg: pkg(),
                error: error(),
            },
            Event::CheckoutRemoveFailed {
                pkg: pkg(),
                error: error(),
            },
            Event::DepsUnresolved {
                pkg: pkg(),
                dependency: "libfoo".to_string(),
            },
            Event::DepsReplaced {
                dependent: pkg(),
                old: PackageRef::from("foo"),
                new: PackageRef::from("bar"),
            },
            Event::DepsDropped {
                dependent: pkg(),
                dependency: PackageRef::from("foo"),
            },
            Event::UpdateSkipped {
                pkg: pkg(),
                error: error(),
            },
            Event::BulkAddResolveFailed { error: error() },
            Event::OperationAborted {
                operation: "bulk_add".to_string(),
                error: error(),
            },
            Event::SettingChanged {
                key: "makepkg_conf".to_string(),
                pkg: Some(pkg()),
            },
            Event::SettingReset {
                key: "makepkg_conf".to_string(),
                pkg: None,
            },
            Event::ScheduleInvalid {
                job: "auto_update".to_string(),
                error: error(),
            },
            Event::SignInRefused {
                user: "mallory@example.com".to_string(),
            },
            Event::TokenRegenerated {},
            Event::DumpExported { secrets: true },
            Event::RestoreApplied { packages: 3 },
            Event::RestorePackageFailed {
                pkg: pkg(),
                step: RestoreStep::Dependencies,
                error: error(),
            },
            Event::RestoreCaReplaced {},
            Event::MirrorRankFailed { error: error() },
            Event::OfficialReposRefreshFailed { error: error() },
            Event::RepoCommitRetried {
                attempt: 2,
                error: error(),
            },
            Event::RepoFileFailed {
                path: "x86_64/hello.pkg.tar.zst".to_string(),
                action: FileAction::Move,
                error: error(),
            },
            Event::RepoSwept {
                files: vec!["hello-1-1.pkg.tar.zst".to_string()],
            },
            Event::RepoSweepFailed { error: error() },
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
            workers: Vec::new(),
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
