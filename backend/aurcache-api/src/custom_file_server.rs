use aurcache_db::helpers::downloads::DownloadCounter;
use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Header, Method, Status};
use rocket::response::Responder;
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route, async_trait, figment};
use std::io::{Cursor, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug, Clone)]
pub struct CustomFileServer {
    root: PathBuf,
    rank: isize,
}

impl CustomFileServer {
    /// The default rank use by `FileServer` routes.
    const DEFAULT_RANK: isize = 10;

    #[track_caller]
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();
        Self {
            root: path.into(),
            rank: Self::DEFAULT_RANK,
        }
    }

    #[must_use]
    pub fn rank(mut self, rank: isize) -> Self {
        self.rank = rank;
        self
    }
}

impl From<CustomFileServer> for Vec<Route> {
    fn from(server: CustomFileServer) -> Self {
        let source = figment::Source::File(server.root.clone());
        let mut route = Route::ranked(server.rank, Method::Get, "/<path..>", server);
        route.name = Some(format!("FileServer: {source}").into());
        vec![route]
    }
}

#[async_trait]
impl Handler for CustomFileServer {
    async fn handle<'r>(&self, req: &'r Request<'_>, data: Data<'r>) -> Outcome<'r> {
        let relative_path = req
            .segments::<Segments<'_, rocket::http::uri::fmt::Path>>(0..)
            .ok()
            // No dot segments: nothing pacman fetches is dot-named, and the
            // repository root also holds uploads being staged for ingest
            // (`.staging`) and artifacts mid-publish, neither of which is
            // anyone's to download.
            .and_then(|segments| segments.to_path_buf(false).ok());

        // Map uri to filepath
        let Some(relative_path) = relative_path else {
            return Outcome::forward(data, Status::NotFound);
        };
        let file_path = self.root.join(relative_path);

        // open file
        let Ok(named_file) = NamedFile::open(&file_path).await else {
            return Outcome::forward(data, Status::NotFound);
        };

        let metadata = named_file.metadata().await.ok();
        let file_size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
        let last_modified = metadata.and_then(|m| m.modified().ok()).map(|mtime| {
            let datetime: chrono::DateTime<chrono::Utc> = mtime.into();
            datetime.to_rfc2822()
        });

        // Counted before the response is built, and only for a whole file.
        // A `Range` request is pacman resuming an interrupted download, and
        // counting each range as a download would inflate a popular package by
        // however many pieces its downloads happened to arrive in. Undercounting
        // resumed downloads is the smaller error of the two.
        if req.method() == Method::Get && req.headers().get_one("Range").is_none() {
            count_download(req, &file_path);
        }

        let mut builder = match get_range_header_data(req, file_size, &file_path).await {
            Some((partial_data, start, end)) => {
                // Build a 206 Partial Content response. The builder methods
                // return `&mut Builder`, so this cannot be a returned chain.
                let mut builder = Response::build();
                builder
                    .status(Status::PartialContent)
                    .raw_header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end - 1, file_size),
                    )
                    .sized_body(partial_data.len(), Cursor::new(partial_data));
                builder
            }
            None => match named_file.respond_to(req) {
                Ok(resp) => Response::build_from(resp),
                Err(_) => return Outcome::error(Status::InternalServerError),
            },
        };

        // Add Headers
        if let Some(lm) = last_modified {
            builder.header(Header::new("Last-Modified", lm));
        }
        builder.header(Header::new("Accept-Ranges", "bytes"));

        Outcome::Success(builder.finalize())
    }
}

/// Record a download of `path`, if it is a package and anyone is counting.
///
/// Every step is optional and silent. This runs inside a static file handler,
/// where the only thing that matters is that the file is served: a missing
/// counter, an unreadable name or a file that is not a package are all reasons
/// to count nothing, and none of them is a reason to fail the request.
fn count_download(req: &Request<'_>, path: &Path) {
    let Some(buffer) = req.rocket().state::<Arc<DownloadCounter>>() else {
        return;
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if is_package_file(name) {
        buffer.record(name);
    }
}

/// Whether a repository file is a built package.
///
/// The repository also serves `repo.db`, `repo.files` and the mirrorlist, and
/// those are fetched on every `pacman -Sy` by every client -- counting them
/// would swamp the figure this exists to report. The compression suffix is left
/// open because it is makepkg's choice, not ours.
fn is_package_file(name: &str) -> bool {
    name.contains(".pkg.tar")
}

/// get range header and read bytes from file
async fn get_range_header_data(
    req: &Request<'_>,
    file_size: u64,
    file_path: &Path,
) -> Option<(Vec<u8>, u64, u64)> {
    let header = req.headers().get_one("Range")?;
    let (start, end) = parse_range_header(header, file_size)?;
    let data = read_file_range(file_path, start, end).await.ok()?;

    Some((data, start, end))
}

/// Parser for Range header in the form "bytes=start-end".
/// Returns a tuple (start, end) where `end` is exclusive.
/// This version does not support multiple ranges.
fn parse_range_header(header: &str, file_size: u64) -> Option<(u64, u64)> {
    if !header.starts_with("bytes=") {
        return None;
    }
    let (start, end) = header[6..].split_once('-')?;
    let start: u64 = start.parse().ok()?;
    // HTTP ranges are inclusive and ours is exclusive; an omitted end means
    // "to the end of the file".
    let end: u64 = match end.parse::<u64>() {
        Ok(e) => e.checked_add(1)?,
        Err(_) => file_size,
    };
    if start >= end || end > file_size {
        return None;
    }
    Some((start, end))
}

/// Reads bytes from `start` up to (but not including) `end` from the file at `path`.
async fn read_file_range(path: &Path, start: u64, end: u64) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let mut buffer = vec![0; usize::try_from(end - start)?];
    file.read_exact(&mut buffer).await?;
    Ok(buffer)
}

#[cfg(test)]
mod download_tests {
    use super::is_package_file;

    /// The repository index is fetched by every client on every `pacman -Sy`.
    /// Counting it would bury the figure this exists to report under traffic
    /// that has nothing to do with any one package.
    #[test]
    fn only_package_files_are_counted() {
        for name in [
            "hello-2.12.1-2-x86_64.pkg.tar.zst",
            "hello-2.12.1-2-x86_64.pkg.tar.xz",
            "lib32-glibc-2.39-1-x86_64.pkg.tar.zst.sig",
        ] {
            assert!(is_package_file(name), "{name} should count");
        }

        for name in [
            "repo.db",
            "repo.db.tar.gz",
            "repo.files",
            "repo.files.tar.gz",
            "mirrorlist.x86_64",
        ] {
            assert!(!is_package_file(name), "{name} should not count");
        }
    }
}
