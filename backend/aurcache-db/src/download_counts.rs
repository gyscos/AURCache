//! How many times each built package file has been downloaded.
//!
//! Keyed by file name rather than by package: the counter is written from the
//! repository file server, which knows the path it just served and nothing
//! else. A file nobody has fetched has no row rather than a zero.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "download_counts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub file_name: String,
    pub count: i64,
    /// Unix seconds of the most recent download, or `None` for a row written
    /// before the column existed.
    pub last_download: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
