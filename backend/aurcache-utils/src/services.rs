use aurcache_db::action::Action;
use aurcache_deps::AurClient;
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::Sender;

use crate::snapshot::SnapshotStore;

/// The four things every package operation needs, passed as one.
///
/// Add, update, bulk add and restore all reach for the same set: the database
/// to write, the build queue to enqueue onto, the snapshot store to resolve
/// sources with, and the AUR client to resolve dependencies with. Threading
/// them one by one put four of the same parameters on every signature in the
/// chain, which is how those signatures grew past what anyone reads.
///
/// Borrowed rather than owned, and `Copy`, so passing it costs what passing
/// the four references cost.
#[derive(Clone, Copy)]
pub struct Services<'a> {
    /// Resolves dependency names against the official repositories and the AUR.
    pub client: &'a AurClient,
    /// Resolves and caches package sources.
    pub store: &'a SnapshotStore,
    pub db: &'a DatabaseConnection,
    /// The build queue.
    pub tx: &'a Sender<Action>,
}

impl<'a> Services<'a> {
    #[must_use]
    pub fn new(
        client: &'a AurClient,
        store: &'a SnapshotStore,
        db: &'a DatabaseConnection,
        tx: &'a Sender<Action>,
    ) -> Self {
        Self {
            client,
            store,
            db,
            tx,
        }
    }
}
