//! One line of the activity log.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How much attention an entry deserves.
///
/// A property of *what happened*, so it is derived from the kind of entry rather
/// than stored beside it: every row already in the table gets a severity the
/// moment the server knows about it, with no column and no backfill. Two events
/// of one kind therefore cannot differ in severity, which is the right shape --
/// "publishing failed" and "package added" are not one event with a field.
/// Stored as the number its variants are ordered by, so "this severity and
/// worse" is `severity >= n` -- one indexable comparison rather than a list of
/// kinds the query would have to know.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "i32", db_type = "Integer")
)]
pub enum Severity {
    /// Something happened. Most of the log.
    #[cfg_attr(feature = "db", sea_orm(num_value = 0))]
    Info,
    /// Something did not work and the server carried on. Worth a look.
    #[cfg_attr(feature = "db", sea_orm(num_value = 1))]
    Warning,
    /// Something did not work and left the instance worse off.
    #[cfg_attr(feature = "db", sea_orm(num_value = 2))]
    Error,
}

impl Severity {
    /// The word this severity is written as, for a URL or a filter.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Read a severity back from [`Self::slug`].
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        [Self::Info, Self::Warning, Self::Error]
            .into_iter()
            .find(|severity| severity.slug() == slug)
    }
}

/// An entry that predates severities is ordinary news.
impl Default for Severity {
    fn default() -> Self {
        Self::Info
    }
}

/// What an entry is about, when it is about something the UI can open.
///
/// Carried beside the text rather than as markup inside it: the server renders
/// prose, and prose with links in it would be the server deciding how the
/// browser lays a page out.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ActivitySubject {
    Package {
        name: String,
    },
    Worker {
        name: String,
    },
    /// One build of a package, which is addressed by both.
    Build {
        pkgbase: String,
        number: i32,
    },
}

impl ActivitySubject {
    /// The token in the entry's text that stands for this subject.
    ///
    /// What the reader clicks, so it is the words the entry actually uses: a
    /// package or a worker goes by name, while a build goes by the `#7` it is
    /// called in the sentence -- the package name is in there too, and linking
    /// that to a build page would send a reader somewhere they did not point.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Package { name } | Self::Worker { name } => name.clone(),
            Self::Build { number, .. } => format!("#{number}"),
        }
    }
}

/// One page of the log, and how long the whole log is.
///
/// The total travels with the page because the log only grows: unlike the
/// package and build lists, there is no "fetch it all and count" that stays
/// cheap, so the pager is told rather than working it out.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ActivityPage {
    pub entries: Vec<Activity>,
    pub total: u64,
}

/// Something that happened, and who did it.
///
/// The text is rendered server-side rather than being a code the frontend has
/// to interpret: the log is prose, and an entry written last year should still
/// read the same after the vocabulary around it changes.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Unix seconds.
    pub timestamp: i64,
    pub text: String,
    /// `None` for anything the server did on its own — a schedule firing, or a
    /// version check — as opposed to a person asking for it.
    pub user: Option<String>,
    /// How much attention this deserves. Absent from an older server's answer,
    /// which reads as [`Severity::Info`] rather than failing the whole listing.
    #[serde(default)]
    pub severity: Severity,
    /// What the entry is about, when that is something with a page of its own.
    ///
    /// `None` for an entry about nothing openable -- and deliberately for a
    /// package that was *removed*, where the only thing to link to is a page
    /// that no longer exists.
    #[serde(default)]
    pub subject: Option<ActivitySubject>,
}
