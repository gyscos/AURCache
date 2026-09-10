use std::ops::Deref;
use std::sync::Arc;

use aurcache_db::action::Action;
use aurcache_deps::AurClient;
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::Request;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use sea_orm::DatabaseConnection;
use tokio::sync::broadcast::Sender;

/// The services a package operation needs, taken from Rocket's state in one
/// guard rather than four parameters per route.
///
/// [`Services`] belongs to `aurcache-utils`, so the guard cannot be
/// implemented on it directly; this wrapper derefs to it, and a route hands
/// `&*services` to anything that takes one.
pub struct ApiServices<'r> {
    services: Services<'r>,
    client: &'r Arc<AurClient>,
    store: &'r Arc<SnapshotStore>,
}

impl<'r> Deref for ApiServices<'r> {
    type Target = Services<'r>;

    fn deref(&self) -> &Self::Target {
        &self.services
    }
}

impl ApiServices<'_> {
    /// The same services, owned, for work that outlives the request.
    ///
    /// A route that spawns cannot hand the task borrowed state; these are the
    /// same instances either way, since the two that matter are behind `Arc`.
    #[must_use]
    pub fn owned(&self) -> OwnedServices {
        OwnedServices {
            client: Arc::clone(self.client),
            store: Arc::clone(self.store),
            db: self.services.db.clone(),
            tx: self.services.tx.clone(),
        }
    }
}

/// [`Services`] with owning handles, for a spawned task.
pub struct OwnedServices {
    pub client: Arc<AurClient>,
    pub store: Arc<SnapshotStore>,
    pub db: DatabaseConnection,
    pub tx: Sender<Action>,
}

impl OwnedServices {
    #[must_use]
    pub fn services(&self) -> Services<'_> {
        Services::new(&self.client, &self.store, &self.db, &self.tx)
    }
}

/// Nothing a request can get wrong: every one of these is managed at launch,
/// and Rocket refuses to start otherwise, so a miss here is a bug rather than
/// a bad request.
#[derive(Debug)]
pub struct MissingState;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ApiServices<'r> {
    type Error = MissingState;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let rocket = req.rocket();
        let (Some(client), Some(store), Some(db), Some(tx)) = (
            rocket.state::<Arc<AurClient>>(),
            rocket.state::<Arc<SnapshotStore>>(),
            rocket.state::<DatabaseConnection>(),
            rocket.state::<Sender<Action>>(),
        ) else {
            return Outcome::Error((Status::InternalServerError, MissingState));
        };

        Outcome::Success(Self {
            services: Services::new(client, store, db, tx),
            client,
            store,
        })
    }
}
