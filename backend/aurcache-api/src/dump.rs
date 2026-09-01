//! Downloading a lite export of the instance's authored state.

use crate::init::ServerVersion;
use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use rocket::http::{Header, Status};
use rocket::{Responder, State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(dump))]
pub struct DumpApi;

/// The archive, with the filename a browser should save it under.
#[derive(Responder)]
#[response(content_type = "application/gzip")]
pub struct DumpArchive {
    body: Vec<u8>,
    disposition: Header<'static>,
}

/// A filename a browser will save sensibly, and that sorts by date in a
/// directory of backups.
fn dump_file_name(created_at: i64) -> String {
    let stamp = chrono::DateTime::from_timestamp(created_at, 0).map_or_else(
        || created_at.to_string(),
        |t| t.format("%Y%m%d").to_string(),
    );
    format!("aurcache-dump-{stamp}.tar.gz")
}

#[utoipa::path(
    responses(
            (status = 200, description = "A .tar.gz of the instance's authored state"),
    )
)]
/// Export packages, settings and approved workers as a `.tar.gz`.
///
/// Authored state only -- build history, logs and the resolved dependency graph
/// are rebuilt by the next resolve and are not carried. No secrets: the CA and
/// worker certificates are deliberately absent, so this is safe to keep
/// alongside ordinary backups.
#[get("/dump")]
pub async fn dump(
    db: &State<DatabaseConnection>,
    version: &State<ServerVersion>,
    _a: Authenticated,
) -> Result<DumpArchive, ApiError> {
    let dump = aurcache_utils::dump::build_dump(db.inner(), &version.0)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let created_at = dump.manifest.created_at;
    let bytes = aurcache_utils::dump::write_archive(&dump)
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(DumpArchive {
        body: bytes,
        disposition: Header::new(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", dump_file_name(created_at)),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::dump_file_name;

    /// Dated, so a directory of backups sorts chronologically and two dumps
    /// taken on different days do not overwrite each other.
    #[test]
    fn the_file_name_carries_the_date() {
        assert_eq!(
            dump_file_name(1_756_684_800),
            "aurcache-dump-20250901.tar.gz"
        );
    }
}
