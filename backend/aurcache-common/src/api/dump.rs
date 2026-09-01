//! The lite export format: what a human authored, not what AURCache worked out.
//!
//! An instance's interesting state is small -- which packages you asked for,
//! how you configured them, which workers you trust. Build history, logs,
//! resolved dependency graphs and the packages themselves are large,
//! reproducible, and left out; the next resolve or build rebuilds them.
//!
//! Nothing here refers to a package by row id. Everything keys on pkgbase,
//! including settings, which the database keys by `pkg_id`. That removes the
//! id-remapping an import would otherwise need, and keeps a dump diffable and
//! mergeable by hand.

use crate::source::SourceData;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// The format version an importer checks before reading anything else.
///
/// The point of the format is restoring into a *different* AURCache version,
/// so an importer must refuse a dump newer than it understands rather than
/// guess at fields it has never seen.
pub const DUMP_SCHEMA_VERSION: u32 = 1;

/// File names within the archive. Named here so the writer and the reader
/// cannot disagree about them.
pub const MANIFEST_FILE: &str = "manifest.json";
pub const PACKAGES_FILE: &str = "packages.json";
pub const SETTINGS_FILE: &str = "settings.json";
pub const WORKERS_FILE: &str = "workers.json";
/// Directory holding one `<pkgbase>.patch` per patched package.
pub const PATCH_DIR: &str = "patches";

#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpManifest {
    /// Checked first, and refused if newer than the importer understands.
    pub schema_version: u32,
    /// Which AURCache wrote it. Informational: the schema version is what
    /// decides compatibility.
    pub aurcache_version: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Whether the archive carries the CA, worker certificates and token
    /// hashes. A dump with this set is dangerous to share -- the CA private
    /// key mints worker identities the server accepts.
    pub includes_secrets: bool,
}

/// One package, as its owner configured it.
///
/// Everything AURCache derives is absent: status, versions, the resolved
/// dependency graph, split package names, provides. A restore rebuilds those.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq)]
pub struct DumpPackage {
    pub source_data: SourceData,
    /// Expanded from the database's semicolon-delimited column, because a dump
    /// is meant to be read and edited by hand.
    pub platforms: Vec<String>,
    pub build_flags: Vec<String>,
    /// False for a package present only because something else needs it. Kept
    /// because a restore that made every package directly-requested would turn
    /// dependencies into things the user has to manage.
    pub directly_requested: bool,
    /// Whether `patches/<pkgbase>.patch` accompanies this entry.
    ///
    /// Not serialised: it is derived from whether the file is there. A flag and
    /// a file can disagree; a file cannot disagree with itself.
    #[serde(skip)]
    pub has_patch: bool,
}

/// Settings, split by what they apply to.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpSettings {
    /// Server-wide values, by setting key.
    pub global: BTreeMap<String, String>,
    /// Per-package overrides, by pkgbase then setting key.
    pub packages: BTreeMap<String, BTreeMap<String, String>>,
}

/// A worker the server trusts, and how work is routed to it.
///
/// The fingerprint is the identity: a worker whose certificate is not in the
/// dump re-enrolls on first contact and is auto-approved because its
/// fingerprint is already known.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpWorker {
    pub name: String,
    pub cert_fingerprint: String,
    pub status: String,
    pub native_arches: Vec<String>,
    pub emulated_arches: Vec<String>,
    pub package_affinity: Vec<String>,
    pub priority: i32,
    pub concurrency: i32,
}

/// `packages.json`: pkgbase to package. A map rather than a list so a dump
/// diffs cleanly when one entry changes.
pub type DumpPackages = BTreeMap<String, DumpPackage>;
