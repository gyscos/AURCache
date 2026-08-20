use crate::activity_serializer::ActivitySerializer;
use crate::package_add_activity::PackageAddActivity;
use crate::package_delete_activity::PackageDeleteActivity;
use crate::package_update_activity::PackageUpdateActivity;
use anyhow::anyhow;
use aurcache_db::activities;
use aurcache_db::activities::ActivityType;
use aurcache_db::prelude::Activities;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, FromQueryResult, Order, QueryOrder,
    QuerySelect,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct Activity {
    pub timestamp: i64,
    pub text: String,
    pub user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActivityLog {
    db: DatabaseConnection,
}

impl ActivityLog {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn add<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) -> anyhow::Result<()> {
        let activity = serde_json::to_string(&activity)?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

        activities::ActiveModel {
            timestamp: Set(timestamp),
            data: Set(activity),
            user: Set(user),
            typ: Set(activity_type),
            ..Default::default()
        }
        .save(&self.db)
        .await?;
        Ok(())
    }

    pub async fn list(&self, limit: Option<u64>) -> anyhow::Result<Vec<Activity>> {
        let activities = Activities::find()
            .order_by(activities::Column::Timestamp, Order::Desc)
            .limit(limit)
            .all(&self.db)
            .await?;

        Ok(activities
            .into_iter()
            .filter_map(|activity| {
                match Self::deserialize_type(activity.typ, &activity.data) {
                    Ok(serializer) => Some(Activity {
                        timestamp: activity.timestamp,
                        text: serializer.format(),
                        user: activity.user,
                    }),
                    Err(e) => {
                        // A row we cannot render is skipped rather than failing the whole listing.
                        tracing::warn!("Skipping unreadable activity row: {e}");
                        None
                    }
                }
            })
            .collect())
    }

    fn deserialize_type(
        activity_type: ActivityType,
        data: &str,
    ) -> anyhow::Result<Box<dyn ActivitySerializer>> {
        Ok(match activity_type {
            ActivityType::AddPackage => Box::new(serde_json::from_str::<PackageAddActivity>(data)?),
            ActivityType::RemovePackage => {
                Box::new(serde_json::from_str::<PackageDeleteActivity>(data)?)
            }
            ActivityType::UpdatePackage => {
                Box::new(serde_json::from_str::<PackageUpdateActivity>(data)?)
            }
            // Nothing writes these types yet; render them as unreadable instead of panicking.
            ActivityType::StartBuild | ActivityType::FinishBuild => {
                return Err(anyhow!("Unsupported activity type: {activity_type:?}"));
            }
        })
    }
}
