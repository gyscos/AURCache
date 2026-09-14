use std::sync::Arc;

use aurcache_db::action::Action;
use aurcache_deps::AurClient;
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::Sender;

use crate::repository::Repository;
use crate::snapshot::SnapshotStore;

/// What a package operation acts through: the database it writes, the build
/// queue it enqueues onto, the source cache it resolves through, and the AUR
/// client it resolves dependencies with.
///
/// This is where those four live, rather than four values threaded separately
/// from `main` and reassembled at every layer. A function that needs more than
/// one of them takes this; a function that needs exactly one takes that one, so
/// its signature still says what it touches.
///
/// Cloning is four refcount bumps -- `DatabaseConnection` and `Sender` are
/// handles and the other two are behind `Arc` -- so a spawned task takes a
/// clone rather than borrowing, and no separate owned form is needed.
#[derive(Clone)]
pub struct Services {
    pub db: DatabaseConnection,
    /// The build queue.
    pub tx: Sender<Action>,
    /// Resolves and caches package sources.
    pub store: Arc<SnapshotStore>,
    /// Resolves dependency names against the official repositories and the AUR.
    pub client: Arc<AurClient>,
    /// The pacman repository, which every change to goes through.
    pub repo: Arc<Repository>,
}

impl Services {
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        tx: Sender<Action>,
        store: Arc<SnapshotStore>,
        client: Arc<AurClient>,
        repo: Arc<Repository>,
    ) -> Self {
        Self {
            db,
            tx,
            store,
            client,
            repo,
        }
    }
}
