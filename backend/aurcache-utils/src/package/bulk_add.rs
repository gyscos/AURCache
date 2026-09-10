//! Adding many packages in one operation.
//!
//! The reason this exists rather than a loop over [`super::add::package_add`]:
//! that path resolves each AUR name to its pkgbase on its own, one RPC request
//! per package, because nothing there ever sees more than one name. Restoring a
//! thousand packages spent a thousand requests before fetching a single source.
//! `resolve_bases` already batches by URL length -- roughly 340 names per
//! request -- so the same thousand names cost three.
//!
//! Everything after that stays per-package: a checkout and a dependency plan
//! cannot be shared. Their cost falls away on its own as the run proceeds,
//! because each added package makes the next one's dependencies resolvable from
//! the database instead of the network.

use std::collections::HashMap;

use aurcache_common::api::package::{BulkAddEntry, BulkAddOutcome};
use aurcache_db::action::Action;
use aurcache_db::packages::SourceData;
use pacman_mirrors::platforms::Platform;
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::package::add::{AddContext, add_resolved_source, build_add_context};
use crate::snapshot::SnapshotStore;

/// How a source was named in the request, for reporting it back.
///
/// A failure has to be attributable to what the caller asked for, and a name
/// that never resolved has no pkgbase to report instead.
fn source_label(source: &SourceData) -> String {
    match source {
        SourceData::Aur { name } => name.clone(),
        SourceData::Git { spec } => spec.url.clone(),
        SourceData::Upload { .. } => "upload".to_string(),
    }
}

/// Resolve every AUR name in `sources` to its pkgbase, in as few requests as
/// the RPC's URL budget allows.
///
/// Names that the AUR does not know are left as they are: they may already
/// *be* pkgbases, which is what the single-package path assumes too, and a
/// name that is simply wrong is better reported by the add that fails on it
/// than by a resolution step guessing.
pub(crate) async fn resolve_pkgbases(
    client: &aurcache_deps::AurClient,
    sources: &[SourceData],
) -> HashMap<String, String> {
    let names: Vec<&str> = sources
        .iter()
        .filter_map(|source| match source {
            SourceData::Aur { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        return HashMap::new();
    }

    match client.resolve_bases(&names).await {
        Ok(bases) => bases,
        Err(e) => {
            // Not fatal: every source falls back to being treated as its own
            // pkgbase, which is what an unresolvable name does anyway. The run
            // continues and reports per-package failures if that was wrong.
            warn!("bulk add could not batch-resolve pkgbases, continuing per package: {e}");
            HashMap::new()
        }
    }
}

/// Add every source, sending each outcome on `progress` as it happens.
///
/// A channel rather than a callback so the caller can persist each outcome as
/// it arrives: a restore runs for minutes, and progress written only at the end
/// is not progress. The receiver being dropped does not stop the run -- nobody
/// watching is a normal state, and the packages still need adding.
///
/// One package failing does not stop the rest either: a restore of a thousand
/// packages should not be undone by one whose PKGBUILD no longer parses, and
/// the entry says which it was.
#[allow(clippy::too_many_arguments)]
pub async fn bulk_add(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    sources: Vec<SourceData>,
    progress: UnboundedSender<BulkAddEntry>,
) {
    let context = build_add_context(platforms, build_flags);
    let bases = resolve_pkgbases(client, &sources).await;

    info!(
        "bulk add: {} sources, {} pkgbases resolved in batch",
        sources.len(),
        bases.len()
    );

    for source in sources {
        let name = source_label(&source);
        let resolved = apply_resolved_base(source, &bases);
        let outcome = add_one(client, store, db, tx, &context, resolved).await;
        // Ignore a closed channel: the observer left, the work has not.
        let _ = progress.send(BulkAddEntry { name, outcome });
    }
}

/// Swap an AUR source's name for its pkgbase, when the batch found one.
fn apply_resolved_base(source: SourceData, bases: &HashMap<String, String>) -> SourceData {
    match source {
        SourceData::Aur { name } => {
            let name = bases.get(&name).cloned().unwrap_or(name);
            SourceData::Aur { name }
        }
        other => other,
    }
}

/// One package's add, with its result turned into an outcome rather than an
/// error, so the caller can record it and carry on.
async fn add_one(
    client: &aurcache_deps::AurClient,
    store: &SnapshotStore,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    context: &AddContext,
    source: SourceData,
) -> BulkAddOutcome {
    let existed = match &source {
        SourceData::Aur { name } => crate::package::add::package_exists(db, name)
            .await
            .unwrap_or(false),
        _ => false,
    };

    match add_resolved_source(client, store, db, tx, context, source, None).await {
        Ok(_) if existed => BulkAddOutcome::Existed,
        Ok(_) => BulkAddOutcome::Added,
        Err(e) => BulkAddOutcome::Failed {
            error: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_resolved_base, resolve_pkgbases, source_label};
    use aurcache_db::packages::SourceData;
    use aurcache_deps::AurClient;
    use std::collections::HashMap;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn aur(name: &str) -> SourceData {
        SourceData::Aur {
            name: name.to_string(),
        }
    }

    /// The reason this module exists: a batch of names costs one request, not
    /// one per name. Restoring a thousand packages spent a thousand requests on
    /// this step before fetching a single source.
    #[tokio::test]
    async fn a_whole_batch_of_names_costs_one_request() {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"type":"multiinfo","resultcount":2,"results":[
                     {"Name":"czkawka-cli","PackageBase":"czkawka","Version":"1.0-1"},
                     {"Name":"hello","PackageBase":"hello","Version":"2.12-1"}
                   ],"version":5}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let sources = vec![aur("czkawka-cli"), aur("hello"), aur("yay")];
        let bases = resolve_pkgbases(&client, &sources).await;

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "each name was resolved on its own instead of in one batch"
        );
        assert_eq!(
            bases.get("czkawka-cli").map(String::as_str),
            Some("czkawka")
        );
    }

    /// Git sources have no pkgbase to look up, so a batch of only those asks
    /// the AUR nothing at all.
    #[tokio::test]
    async fn git_sources_alone_ask_the_aur_nothing() {
        let server = MockServer::start().await;
        let client = AurClient::with_urls(format!("{}/rpc/v5", server.uri()));
        let sources = vec![SourceData::Git {
            spec: aurcache_common::source::GitSourceSpec {
                url: "https://example.com/x.git".to_string(),
                r#ref: "main".to_string(),
                subfolder: String::new(),
            },
        }];

        assert!(resolve_pkgbases(&client, &sources).await.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    /// A name the AUR does not know keeps its own spelling: it may already be a
    /// pkgbase, which is what the single-package path assumes too. Guessing
    /// otherwise here would turn a typo into a confusing resolution error
    /// instead of a plain "package not found" from the add.
    #[test]
    fn an_unresolved_name_is_left_alone() {
        let bases = HashMap::from([("czkawka-cli".to_string(), "czkawka".to_string())]);
        assert_eq!(
            source_label(&apply_resolved_base(aur("czkawka-cli"), &bases)),
            "czkawka"
        );
        assert_eq!(
            source_label(&apply_resolved_base(aur("unknown"), &bases)),
            "unknown"
        );
    }
}
