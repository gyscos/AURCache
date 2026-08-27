//! One line of the activity log.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Something that happened, and who did it.
///
/// The text is rendered server-side rather than being a code the frontend has
/// to interpret: the log is prose, and an entry written last year should still
/// read the same after the vocabulary around it changes.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct Activity {
    /// Unix seconds.
    pub timestamp: i64,
    pub text: String,
    /// `None` for anything the server did on its own — a schedule firing, or a
    /// version check — as opposed to a person asking for it.
    pub user: Option<String>,
}
