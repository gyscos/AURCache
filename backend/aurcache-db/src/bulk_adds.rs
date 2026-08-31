//! One bulk package-add and its progress. See the migration for why it exists.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "bulk_adds")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub created_at: i64,
    /// `None` while the job is still running.
    pub finished_at: Option<i64>,
    /// How many sources the request carried, fixed when the job is created.
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
    /// Append-only, one JSON outcome per line, read by line offset.
    pub log: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
