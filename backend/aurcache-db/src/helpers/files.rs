//! Queries over a package's built artifacts.

use crate::{files, packages};
use sea_orm::sea_query::{Alias, Asterisk, CaseStatement, Expr, ExprTrait, Query};

/// Total size of every artifact belonging to the package row in the *enclosing*
/// query, as a correlated scalar subquery.
///
/// All-or-nothing, matching what the package page applies to its own total: the
/// `CASE` yields NULL unless every artifact has a recorded size, so a partial
/// sum never reaches the column -- a sum of only the known parts reads as a
/// wrong number rather than as missing data. A package with no artifacts sums
/// over no rows and is NULL too, which is what "nothing built yet" should show.
///
/// Cast to `BIGINT` because Postgres widens `SUM(bigint)` to `numeric`, which
/// does not decode into an `i64`; SQLite reads the cast as its own INTEGER
/// affinity and is unaffected.
#[must_use]
pub fn total_artifact_size_expr() -> Expr {
    Expr::from(
        Query::select()
            .expr(
                CaseStatement::new().case(
                    Expr::col(Asterisk)
                        .count()
                        .eq(Expr::col((files::Entity, files::Column::Size)).count()),
                    Expr::col((files::Entity, files::Column::Size))
                        .sum()
                        .cast_as(Alias::new("BIGINT")),
                ),
            )
            .from(files::Entity)
            .and_where(
                Expr::col((files::Entity, files::Column::PackageId))
                    .equals((packages::Entity, packages::Column::Id)),
            )
            .to_owned(),
    )
}
