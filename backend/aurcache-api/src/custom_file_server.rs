use aurcache_db::helpers::downloads::DownloadCounter;
use rocket::fs::NamedFile;
use rocket::http::uri::Segments;
use rocket::http::{Header, Method, Status};
use rocket::response::Responder;
use rocket::route::{Handler, Outcome};
use rocket::{Data, Request, Response, Route, async_trait, figment};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncSeekExt;

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

        // A range starting past the end is 416 with `Content-Range: bytes */size`,
        // which resuming clients (pacman) expect, rather than a silent 200. A
        // header that is not a single byte range is ignored, as HTTP says, and
        // the whole file is sent.
        let range = req
            .headers()
            .get_one("Range")
            .map_or(RangeRequest::Ignored, |h| parse_range_header(h, file_size));
        let mut builder = match range {
            RangeRequest::Ignored => match named_file.respond_to(req) {
                Ok(resp) => Response::build_from(resp),
                Err(_) => return Outcome::error(Status::InternalServerError),
            },
            RangeRequest::Satisfiable(start, end) => {
                match range_body(&file_path, start, end).await {
                    Ok((len, body)) => {
                        // Build a 206 Partial Content response. The builder
                        // methods return `&mut Builder`, so this cannot be a
                        // returned chain.
                        let mut builder = Response::build();
                        builder
                            .status(Status::PartialContent)
                            .raw_header(
                                "Content-Range",
                                format!("bytes {}-{}/{}", start, end - 1, file_size),
                            )
                            .sized_body(len, body);
                        builder
                    }
                    Err(_) => return Outcome::error(Status::InternalServerError),
                }
            }
            RangeRequest::Unsatisfiable => {
                let mut builder = Response::build();
                builder
                    .status(Status::RangeNotSatisfiable)
                    .raw_header("Content-Range", format!("bytes */{file_size}"));
                builder
            }
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

/// Open the requested byte range as a streaming body.
///
/// The file is seeked and handed to the response unread: a `bytes=0-` over a
/// multi-GB package must never become a gigabyte `Vec` in RAM.
async fn range_body(file_path: &Path, start: u64, end: u64) -> anyhow::Result<(usize, File)> {
    let mut file = File::open(file_path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    Ok((usize::try_from(end - start)?, file))
}

/// What a `Range` header asks of a file of a given size.
#[derive(Debug, PartialEq, Eq)]
enum RangeRequest {
    /// Send `start..end` (exclusive) as 206.
    Satisfiable(u64, u64),
    /// A valid range the file cannot satisfy: 416.
    Unsatisfiable,
    /// Not a single byte range this server understands -- another unit, a
    /// multi-range, malformed syntax. HTTP says to ignore the header and send
    /// the whole file.
    Ignored,
}

/// Parse a `Range` header of the form `bytes=start-end`, `bytes=start-` or
/// `bytes=-suffix` (RFC 9110 §14.1.2).
///
/// An end past the file is clamped rather than refused: a client may ask for
/// more than there is, and gets what there is. Only a start at or past the end
/// (or an empty suffix) is unsatisfiable.
fn parse_range_header(header: &str, file_size: u64) -> RangeRequest {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeRequest::Ignored;
    };
    if spec.contains(',') {
        return RangeRequest::Ignored;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeRequest::Ignored;
    };
    let (first, last) = (first.trim(), last.trim());

    if first.is_empty() {
        // The last `n` bytes.
        return match last.parse::<u64>() {
            Ok(0) => RangeRequest::Unsatisfiable,
            Ok(_) if file_size == 0 => RangeRequest::Unsatisfiable,
            Ok(n) => RangeRequest::Satisfiable(file_size.saturating_sub(n), file_size),
            Err(_) => RangeRequest::Ignored,
        };
    }

    let Ok(start) = first.parse::<u64>() else {
        return RangeRequest::Ignored;
    };
    // Inclusive on the wire, exclusive here; an omitted end is "to the end".
    let end = if last.is_empty() {
        file_size
    } else {
        match last.parse::<u64>() {
            Ok(inclusive) if inclusive < start => return RangeRequest::Ignored,
            Ok(inclusive) => inclusive.saturating_add(1).min(file_size),
            Err(_) => return RangeRequest::Ignored,
        }
    };
    if start >= file_size {
        return RangeRequest::Unsatisfiable;
    }
    RangeRequest::Satisfiable(start, end)
}

#[cfg(test)]
mod download_tests {
    use super::{RangeRequest, is_package_file, parse_range_header};

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

    /// The parser speaks exclusive ends: HTTP ranges are inclusive, and an
    /// omitted end means "to the end of the file".
    #[test]
    fn range_bounds_are_exclusive_with_open_end() {
        use RangeRequest::Satisfiable;
        assert_eq!(parse_range_header("bytes=0-99", 1000), Satisfiable(0, 100));
        assert_eq!(
            parse_range_header("bytes=100-", 1000),
            Satisfiable(100, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=500-500", 1000),
            Satisfiable(500, 501)
        );
    }

    /// Asking for more than there is gets what there is, and a suffix is the
    /// last bytes of the file -- neither is an error.
    #[test]
    fn long_and_suffix_ranges_are_satisfied() {
        use RangeRequest::Satisfiable;
        assert_eq!(
            parse_range_header("bytes=0-9999", 1000),
            Satisfiable(0, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=-100", 1000),
            Satisfiable(900, 1000)
        );
        assert_eq!(
            parse_range_header("bytes=-5000", 1000),
            Satisfiable(0, 1000)
        );
    }

    /// Only a range starting past the end is answered 416.
    #[test]
    fn a_start_past_the_end_is_unsatisfiable() {
        use RangeRequest::Unsatisfiable;
        assert_eq!(parse_range_header("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=9999-", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range_header("bytes=-10", 0), Unsatisfiable);
    }

    /// What is not a single byte range is ignored, so the whole file is sent.
    #[test]
    fn other_ranges_are_ignored() {
        use RangeRequest::Ignored;
        assert_eq!(parse_range_header("bytes=600-100", 1000), Ignored);
        assert_eq!(parse_range_header("items=0-99", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=abc-def", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=0-abc", 1000), Ignored);
        assert_eq!(parse_range_header("bytes=0-9,20-29", 1000), Ignored);
    }
}
