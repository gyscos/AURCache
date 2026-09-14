//! The pacman repository this server publishes: the package files, the
//! `repo.db`/`repo.files` databases indexing them, the `files` rows recording
//! who publishes what, and the staging area uploads wait in. Every change to it
//! goes through [`Repository::begin`].

use aurcache_db::files;
use aurcache_db::prelude::Files;
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::PackageEntry;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, warn};

/// Where the repository lives, relative to the server's working directory.
pub const REPO_ROOT: &str = "./repo";

/// Where uploads wait to be published, under the repository root.
///
/// Dot-named, and the repository's file server refuses dot segments, so an
/// upload is never downloadable before it is published.
const STAGING_DIR: &str = ".staging";

const DB_ARCHIVE: &str = "repo.db.tar.gz";
const FILES_ARCHIVE: &str = "repo.files.tar.gz";

/// How many times a failed `commit` is tried in all. Enough to ride out a
/// database that is briefly busy; a failure that will not go away just fails a
/// few times quickly.
const COMMIT_ATTEMPTS: u32 = 3;

/// The repository on disk.
///
/// One per server. Every change is serialized by its lock, covering the whole
/// read-modify-write of the databases: two publishes on the same platform each
/// starting from the databases as they were would each drop the other's entry
/// when renaming its own result in, however atomic the rename.
pub struct Repository {
    root: PathBuf,
    lock: tokio::sync::Mutex<()>,
    /// Numbers each update, so a [`PublishedFile`] can say which one read it.
    updates: AtomicU64,
    /// When each retired file stopped being listed, as far as this process
    /// knows: set by the update that retired it, or by the first sweep that
    /// found it unlisted. Kept in memory only -- after a restart a retired
    /// file's grace period starts again, which can only keep it longer.
    retired: std::sync::Mutex<HashMap<PathBuf, Instant>>,
}

/// A package file the repository publishes, as its `files` row records it.
///
/// Only an [`Update`] reads these, and [`Update::retire`] only takes these, from
/// the same update: a file is only ever retired on the strength of a read made
/// under the repository lock, by the change doing the retiring. A read made
/// before the lock, or by an earlier change, could be out of date -- another
/// package may have claimed the file since -- and acting on it would retire that
/// package's file.
#[derive(Debug)]
pub struct PublishedFile {
    update: u64,
    row: files::Model,
}

impl PublishedFile {
    /// The `files` row's id.
    #[must_use]
    pub fn id(&self) -> i32 {
        self.row.id
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.row.filename
    }

    #[must_use]
    pub fn platform(&self) -> Platform {
        self.row.platform
    }

    /// The package publishing it.
    #[must_use]
    pub fn package_id(&self) -> i32 {
        self.row.package_id
    }
}

/// What one update adds and retires.
#[derive(Default)]
struct Changes {
    add: Vec<Addition>,
    retire: Vec<(Platform, String)>,
}

struct Addition {
    platform: Platform,
    staged: PathBuf,
    entry: PackageEntry,
}

impl Changes {
    fn add(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.add.push(Addition {
            platform,
            staged,
            entry,
        });
    }

    fn retire(&mut self, platform: Platform, filename: impl Into<String>) {
        self.retire.push((platform, filename.into()));
    }

    fn platforms(&self) -> Vec<Platform> {
        let mut platforms: Vec<Platform> = self
            .add
            .iter()
            .map(|a| a.platform)
            .chain(self.retire.iter().map(|(platform, _)| *platform))
            .collect();
        platforms.sort_by_key(Platform::as_str);
        platforms.dedup();
        platforms
    }
}

/// One change to the repository, holding its lock (the `_lock` field). See
/// [`Repository::begin`].
///
/// Reading what the repository publishes, to decide what to change, goes
/// through this too: the reads are only available while the lock is held.
pub struct Update<'a> {
    repo: &'a Repository,
    /// Which update this is, so a [`PublishedFile`] read by another one is
    /// refused.
    id: u64,
    /// The repository lock: the guard of [`Repository`]'s mutex, taken by
    /// [`Repository::begin`] and released when the update is committed or
    /// dropped. Never read, only held -- hence the underscore.
    ///
    /// It is exclusive *write* access to the repository, which is three things
    /// kept in step:
    ///
    /// - each platform's `repo.db` and `repo.files`, rewritten as a whole from
    ///   their current contents (and through fixed `.next` names beside them);
    /// - the package files in each platform directory;
    /// - the `files` rows saying which package publishes which of those files.
    ///
    /// So anything that changes one of these holds it, and so does anything
    /// that *decides* a change from what they currently say -- which is why the
    /// `files` reads a change is based on are only available from an update.
    /// Without it, two changes each start from the same state and the second
    /// undoes the first: an entry dropped from `repo.db`, or a file removed on
    /// the strength of a row another package has since claimed.
    ///
    /// Reading needs no lock, because every write is atomic from a reader's
    /// side: a database is replaced by renaming a complete new one over it, a
    /// package file is renamed into place, and the `files` rows change in one
    /// transaction. A client or the web UI sees the repository before a change
    /// or after it. Order does the rest: new package files land before the
    /// databases listing them, and a retired file stays downloadable for a
    /// grace period after the databases stop listing it, for clients still
    /// holding an older `repo.db` (see [`Repository::sweep`]).
    ///
    /// Not covered: the staging directories, which each belong to one build
    /// and are written by its uploads without the lock; every other table; and
    /// anything outside this process -- it is an in-process mutex, so a second
    /// server on the same repository, or a hand edit, is not serialized.
    _lock: tokio::sync::MutexGuard<'a, ()>,
    /// What the change adds and retires, applied by [`Update::commit`].
    changes: Changes,
}

impl Update<'_> {
    /// The file `platform` publishes as `filename`, if any.
    pub async fn published_file<C: ConnectionTrait>(
        &self,
        db: &C,
        platform: Platform,
        filename: &str,
    ) -> anyhow::Result<Option<PublishedFile>> {
        let row = Files::find()
            .filter(files::Column::Filename.eq(filename))
            .filter(files::Column::Platform.eq(platform))
            .one(db)
            .await?;
        Ok(row.map(|row| self.published(row)))
    }

    /// Every file the packages `pkg_ids` publish, on every platform.
    pub async fn published_files_of<C: ConnectionTrait>(
        &self,
        db: &C,
        pkg_ids: &[i32],
    ) -> anyhow::Result<Vec<PublishedFile>> {
        let rows = Files::find()
            .filter(files::Column::PackageId.is_in(pkg_ids.iter().copied()))
            .all(db)
            .await?;
        Ok(rows.into_iter().map(|row| self.published(row)).collect())
    }

    /// Every file the repository publishes.
    pub async fn all_published_files<C: ConnectionTrait>(
        &self,
        db: &C,
    ) -> anyhow::Result<Vec<PublishedFile>> {
        let rows = Files::find().all(db).await?;
        Ok(rows.into_iter().map(|row| self.published(row)).collect())
    }

    /// The recorded changes, leaving none behind, for [`Update::commit`] to
    /// apply.
    fn take_changes(&mut self) -> Changes {
        std::mem::take(&mut self.changes)
    }

    fn published(&self, row: files::Model) -> PublishedFile {
        PublishedFile {
            update: self.id,
            row,
        }
    }

    /// Publish the package file `entry` describes, moving it from `staged`.
    pub fn add(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.changes.add(platform, staged, entry);
    }

    /// Take a published file out of the repository.
    ///
    /// Its database entry goes with this update. The file itself, and its
    /// detached signature, stay downloadable until [`Repository::sweep`]
    /// finds them unlisted for longer than the grace period: a client that
    /// synced just before this update still asks for them.
    ///
    /// Refused for a file this update did not read itself; see
    /// [`PublishedFile`].
    pub fn retire(&mut self, file: &PublishedFile) -> anyhow::Result<()> {
        if file.update != self.id {
            anyhow::bail!(
                "{} was read by another repository update; only the update that read it may retire it",
                file.filename()
            );
        }
        self.changes.retire(file.platform(), file.filename());
        Ok(())
    }

    /// Make the recorded changes, with `commit` -- the caller's database
    /// transaction -- as the decision point:
    ///
    /// 1. The new databases are written beside the current ones.
    /// 2. `commit` runs, and is tried again if it fails.
    /// 3. Only once it has succeeded: staged files are moved in and the new
    ///    databases renamed over the old. Retired files stay for the sweep.
    ///
    /// What can fail and is private comes first, then the riskiest public step,
    /// and last the renames. A failure before step 3 leaves the repository
    /// exactly as it was and is returned. Step 3 is past the point of no return
    /// -- the database already says it happened -- so its failures are logged
    /// rather than returned.
    pub async fn commit<T, F, Fut>(mut self, mut commit: F) -> anyhow::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        // `self`, and with it the lock, lives until this function returns:
        // every step below is part of the one serialized change.
        let root = self.repo.root();
        let changes = self.take_changes();

        if let Some(addition) = changes.add.iter().find(|a| !a.staged.starts_with(root)) {
            anyhow::bail!(
                "{} is not staged under the repository at {}",
                addition.staged.display(),
                root.display()
            );
        }

        let changes = Arc::new(changes);
        let prepared = {
            let root = root.to_path_buf();
            let changes = Arc::clone(&changes);
            tokio::task::spawn_blocking(move || prepare(&root, &changes)).await??
        };

        let mut attempt = 1;
        let committed = loop {
            match commit().await {
                Ok(done) => break done,
                Err(e) if attempt < COMMIT_ATTEMPTS => {
                    warn!("repository update did not commit (attempt {attempt}): {e:#}");
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                    attempt += 1;
                }
                Err(e) => {
                    for p in &prepared {
                        p.discard();
                    }
                    return Err(e);
                }
            }
        };

        let root = root.to_path_buf();
        match tokio::task::spawn_blocking(move || publish(&root, &changes, &prepared)).await {
            Ok(retired) => self.repo.note_retired(retired, Instant::now()),
            Err(e) => error!("publishing a committed repository update panicked: {e}"),
        }
        Ok(committed)
    }
}

/// The new databases for one platform, written beside the current ones.
struct Prepared {
    db: PathBuf,
    db_next: PathBuf,
    files: PathBuf,
    files_next: PathBuf,
}

impl Prepared {
    fn discard(&self) {
        let _ = std::fs::remove_file(&self.db_next);
        let _ = std::fs::remove_file(&self.files_next);
    }
}

impl Repository {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock: tokio::sync::Mutex::new(()),
            updates: AtomicU64::new(0),
            retired: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a build's artifacts are uploaded to and wait until published.
    ///
    /// Inside the repository root by construction: publishing moves the files
    /// into place with a rename, which needs both ends on one filesystem, and
    /// deployments mount the repository as a volume of its own.
    #[must_use]
    pub fn staging_dir(&self, build_id: i32) -> PathBuf {
        self.root.join(STAGING_DIR).join(build_id.to_string())
    }

    /// Read a staged package into its database entry. The slow part of
    /// publishing -- the whole archive is read -- and it changes nothing, so it
    /// needs no lock and belongs before [`Repository::begin`].
    pub async fn describe(&self, staged: PathBuf) -> anyhow::Result<PackageEntry> {
        tokio::task::spawn_blocking(move || pacman_repo_utils::describe_package(&staged)).await?
    }

    /// Start changing the repository.
    ///
    /// Takes the repository lock, which the returned update holds until it is
    /// committed or dropped. What the repository publishes is read through the
    /// update, so every decision about what to change is made with no other
    /// change able to interleave. Dropping it without committing changes
    /// nothing.
    pub async fn begin(&self) -> Update<'_> {
        let lock = self.lock.lock().await;
        Update {
            repo: self,
            id: self.updates.fetch_add(1, Ordering::Relaxed),
            _lock: lock,
            changes: Changes::default(),
        }
    }

    /// Delete the retired package files -- present in a platform directory, not
    /// listed by its `repo.db` -- that have been unlisted for at least
    /// `grace`. Returns how many files it deleted.
    ///
    /// Unlisted by `repo.db` rather than missing from the `files` table: if the
    /// database were ever wrong wholesale (restored from an old dump, or the
    /// wrong one), every file would look unpublished and the whole repository
    /// would go. `repo.db` lives beside the files and only ever changes through
    /// an update, atomically. A platform directory without a readable
    /// `repo.db` is skipped for the same reason.
    ///
    /// Takes the repository lock: deciding what to delete from what is listed
    /// is a change like any other, and an update in flight may be about to list
    /// a file this would otherwise see as unlisted.
    pub async fn sweep(&self, grace: Duration) -> anyhow::Result<usize> {
        self.sweep_at(Instant::now(), grace).await
    }

    async fn sweep_at(&self, now: Instant, grace: Duration) -> anyhow::Result<usize> {
        let _lock = self.lock.lock().await;
        let root = self.root.clone();
        let known = std::mem::take(&mut *self.retired_times());
        let (still_retired, deleted) =
            tokio::task::spawn_blocking(move || sweep_retired(&root, known, now, grace)).await?;
        *self.retired_times() = still_retired;
        Ok(deleted)
    }

    fn note_retired(&self, paths: Vec<PathBuf>, when: Instant) {
        let mut retired = self.retired_times();
        for path in paths {
            retired.insert(path, when);
        }
    }

    fn retired_times(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Instant>> {
        self.retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn platform_dir(root: &Path, platform: Platform) -> PathBuf {
        root.join(platform.as_str())
    }
}

/// Step 1: the new databases of every platform the changes touch, beside the
/// current ones. Dot-named, so never served; discarded if anything fails.
fn prepare(root: &Path, changes: &Changes) -> anyhow::Result<Vec<Prepared>> {
    let mut prepared = Vec::new();
    for platform in changes.platforms() {
        let dir = Repository::platform_dir(root, platform);
        let result = (|| {
            std::fs::create_dir_all(&dir)?;
            let p = Prepared {
                db: dir.join(DB_ARCHIVE),
                db_next: dir.join(format!(".{DB_ARCHIVE}.next")),
                files: dir.join(FILES_ARCHIVE),
                files_next: dir.join(format!(".{FILES_ARCHIVE}.next")),
            };
            let remove: Vec<String> = changes
                .retire
                .iter()
                .filter(|(p, _)| *p == platform)
                .map(|(_, filename)| filename.clone())
                .collect();
            let add: Vec<PackageEntry> = changes
                .add
                .iter()
                .filter(|a| a.platform == platform)
                .map(|a| a.entry.clone())
                .collect();
            let written = pacman_repo_utils::write_updated_databases(
                &p.db,
                &p.files,
                &remove,
                &add,
                &p.db_next,
                &p.files_next,
            );
            if let Err(e) = written {
                p.discard();
                return Err(e);
            }
            Ok(p)
        })();
        match result {
            Ok(p) => prepared.push(p),
            Err(e) => {
                for p in &prepared {
                    p.discard();
                }
                return Err(e.context(format!("preparing the {platform} databases")));
            }
        }
    }
    Ok(prepared)
}

/// Step 3: make a committed update visible, and return the files it retired.
///
/// Package files first, so everything the new databases list exists before
/// they do; the databases next. Retired files are left where they are.
fn publish(root: &Path, changes: &Changes, prepared: &[Prepared]) -> Vec<PathBuf> {
    let mut added: HashSet<(Platform, &str)> = HashSet::new();
    for addition in &changes.add {
        let dir = Repository::platform_dir(root, addition.platform);
        let filename = addition.entry.filename.as_str();
        added.insert((addition.platform, filename));
        rename_logged(&addition.staged, &dir.join(filename));
        let staged_sig = with_suffix(&addition.staged, ".sig");
        if staged_sig.exists() {
            rename_logged(&staged_sig, &dir.join(format!("{filename}.sig")));
        }
    }
    for p in prepared {
        rename_logged(&p.db_next, &p.db);
        rename_logged(&p.files_next, &p.files);
    }
    let mut retired = Vec::new();
    for (platform, filename) in &changes.retire {
        // A rebuild at the same version replaces the file it retires: the new
        // one is already in place, and listed, under that name.
        if added.contains(&(*platform, filename.as_str())) {
            continue;
        }
        let path = Repository::platform_dir(root, *platform).join(filename);
        retired.push(with_suffix(&path, ".sig"));
        retired.push(path);
    }
    retired
}

/// Whether a name in a platform directory is a package file or a package's
/// detached signature -- the only things a sweep ever considers.
fn is_package_file(name: &str) -> bool {
    !name.starts_with('.') && name.contains(".pkg.tar.")
}

/// [`Repository::sweep`], given what is known of when files were retired.
/// Returns the retirement times still worth keeping, and how many files it
/// deleted.
fn sweep_retired(
    root: &Path,
    mut known: HashMap<PathBuf, Instant>,
    now: Instant,
    grace: Duration,
) -> (HashMap<PathBuf, Instant>, usize) {
    let mut still_retired = HashMap::new();
    let mut deleted = 0;
    let Ok(platforms) = std::fs::read_dir(root) else {
        return (still_retired, deleted);
    };
    for platform in platforms.flatten() {
        let dir = platform.path();
        let hidden = platform.file_name().to_string_lossy().starts_with('.');
        if hidden || !dir.is_dir() {
            continue;
        }
        let listed = match pacman_repo_utils::listed_filenames(&dir.join(DB_ARCHIVE)) {
            Ok(listed) => listed,
            Err(e) => {
                warn!(
                    "not sweeping {}: its repo.db cannot be read ({e})",
                    dir.display()
                );
                continue;
            }
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_package_file(&name) {
                continue;
            }
            let package = name.strip_suffix(".sig").unwrap_or(&name);
            if listed.contains(package) {
                continue;
            }
            let path = entry.path();
            let since = known.remove(&path).unwrap_or(now);
            if now.saturating_duration_since(since) >= grace {
                remove_logged(&path);
                deleted += 1;
            } else {
                still_retired.insert(path, since);
            }
        }
    }
    (still_retired, deleted)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn rename_logged(from: &Path, to: &Path) {
    if let Err(e) = std::fs::rename(from, to) {
        error!("could not move {} to {}: {e}", from.display(), to.display());
    }
}

fn remove_logged(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!("deleted retired {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A minimal valid `.pkg.tar.zst`: a zstd-compressed tar holding a
    /// `.PKGINFO` with a pkgname and pkgver, which is all a database entry needs.
    pub(crate) fn fake_pkg_zst(pkgname: &str, pkgver: &str) -> Vec<u8> {
        let pkginfo = format!("pkgname = {pkgname}\npkgver = {pkgver}\n");
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(".PKGINFO").unwrap();
            header.set_size(pkginfo.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, pkginfo.as_bytes()).unwrap();
            builder.finish().unwrap();
        }
        zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
    }

    fn filename(pkgname: &str, pkgver: &str) -> String {
        format!("{pkgname}-{pkgver}-x86_64.pkg.tar.zst")
    }

    /// Stage a fake package for build `build_id` and describe it.
    async fn stage(
        repo: &Repository,
        build_id: i32,
        pkgname: &str,
        pkgver: &str,
    ) -> (PathBuf, PackageEntry) {
        let dir = repo.staging_dir(build_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename(pkgname, pkgver));
        std::fs::write(&path, fake_pkg_zst(pkgname, pkgver)).unwrap();
        let entry = repo.describe(path.clone()).await.unwrap();
        (path, entry)
    }

    /// The entry directories `repo.db` lists for x86_64.
    fn listed(repo: &Repository) -> Vec<String> {
        let path = repo.root().join("x86_64").join(DB_ARCHIVE);
        let mut data = Vec::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data.as_slice()));
        let mut dirs: Vec<String> = archive
            .entries()
            .unwrap()
            .flatten()
            .filter(|e| e.header().entry_type().is_dir())
            .map(|e| e.path().unwrap().display().to_string())
            .collect();
        dirs.sort();
        dirs
    }

    fn next_files_left(repo: &Repository) -> Vec<String> {
        std::fs::read_dir(repo.root().join("x86_64"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".next"))
            .collect()
    }

    /// The x86_64 file `filename`, as `update` would have read it.
    fn published(update: &Update<'_>, filename: &str) -> PublishedFile {
        update.published(files::Model {
            filename: filename.to_string(),
            id: 0,
            platform: Platform::X86_64,
            package_id: 0,
            size: None,
        })
    }

    /// A file read by one update cannot be retired by another: by then it may
    /// belong to someone else.
    #[tokio::test]
    async fn a_file_read_by_another_update_cannot_be_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let stale = {
            let update = repo.begin().await;
            published(&update, &filename("foo", "1.0-1"))
        };

        let mut update = repo.begin().await;
        assert!(update.retire(&stale).is_err());
        let fresh = published(&update, &filename("foo", "1.0-1"));
        assert!(update.retire(&fresh).is_ok());
    }

    /// Commit a change that the database accepts.
    async fn apply(
        repo: &Repository,
        add: Vec<(PathBuf, PackageEntry)>,
        remove: &[String],
    ) -> anyhow::Result<()> {
        let mut update = repo.begin().await;
        for (staged, entry) in add {
            update.add(Platform::X86_64, staged, entry);
        }
        for filename in remove {
            let file = published(&update, filename);
            update.retire(&file).unwrap();
        }
        update.commit(|| async { Ok(()) }).await
    }

    #[tokio::test]
    async fn an_update_publishes_its_additions_and_retirements() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged.clone(), entry)], &[])
            .await
            .unwrap();
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
        assert!(platform_file(&repo, "foo", "1.0-1").exists());
        assert!(!staged.exists(), "the staged file is moved, not copied");

        let (staged, entry) = stage(&repo, 2, "foo", "1.1-1").await;
        apply(&repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
            .await
            .unwrap();
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
        assert!(
            platform_file(&repo, "foo", "1.0-1").exists(),
            "a retired file stays downloadable until the sweep"
        );
        assert!(next_files_left(&repo).is_empty());
    }

    fn platform_file(repo: &Repository, pkgname: &str, pkgver: &str) -> PathBuf {
        repo.root().join("x86_64").join(filename(pkgname, pkgver))
    }

    /// Publish foo 1.0 with a detached signature, then 1.1 over it: 1.0 and
    /// its signature are retired.
    async fn retire_one(repo: &Repository) {
        let (staged, entry) = stage(repo, 1, "foo", "1.0-1").await;
        apply(repo, vec![(staged, entry)], &[]).await.unwrap();
        std::fs::write(
            with_suffix(&platform_file(repo, "foo", "1.0-1"), ".sig"),
            b"sig",
        )
        .unwrap();
        let (staged, entry) = stage(repo, 2, "foo", "1.1-1").await;
        apply(repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
            .await
            .unwrap();
    }

    /// A retired file, and its signature, go once unlisted for the grace
    /// period -- not before, and the published version is never touched.
    #[tokio::test]
    async fn the_sweep_deletes_retired_files_once_their_grace_is_over() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let old = platform_file(&repo, "foo", "1.0-1");
        let grace = Duration::from_secs(3600);

        assert_eq!(repo.sweep(grace).await.unwrap(), 0);
        assert!(old.exists() && with_suffix(&old, ".sig").exists());

        let later = Instant::now() + grace;
        assert_eq!(repo.sweep_at(later, grace).await.unwrap(), 2);
        assert!(!old.exists());
        assert!(!with_suffix(&old, ".sig").exists());
        assert!(platform_file(&repo, "foo", "1.1-1").exists());
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
    }

    /// A restart forgets when files were retired. The first sweep after it
    /// starts their grace then, so a restart can only keep a file longer.
    #[tokio::test]
    async fn after_a_restart_a_retired_file_gets_its_grace_again() {
        let tmp = tempfile::tempdir().unwrap();
        let grace = Duration::from_secs(3600);
        retire_one(&Repository::new(tmp.path())).await;

        let restarted = Repository::new(tmp.path());
        let old = platform_file(&restarted, "foo", "1.0-1");
        // Long after the retirement, but the first this process has seen of it.
        assert_eq!(restarted.sweep(grace).await.unwrap(), 0);
        assert!(old.exists());

        let later = Instant::now() + grace;
        assert_eq!(restarted.sweep_at(later, grace).await.unwrap(), 2);
        assert!(!old.exists());
    }

    /// A file published again under the same name before the sweep is listed
    /// once more, and stays.
    #[tokio::test]
    async fn a_file_listed_again_is_not_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let (staged, entry) = stage(&repo, 3, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();

        let later = Instant::now() + Duration::from_secs(7200);
        repo.sweep_at(later, Duration::from_secs(3600))
            .await
            .unwrap();

        assert!(platform_file(&repo, "foo", "1.0-1").exists());
        assert_eq!(listed(&repo), ["foo-1.0-1", "foo-1.1-1"]);
    }

    /// The sweep only ever deletes package files a readable `repo.db` does not
    /// list: not the databases or anything else beside them, and nothing at
    /// all in a directory whose `repo.db` is missing.
    #[tokio::test]
    async fn the_sweep_leaves_everything_but_unlisted_packages_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let x86 = repo.root().join("x86_64");
        std::fs::write(x86.join("mirrorlist"), b"Server = x").unwrap();
        let orphan_dir = repo.root().join("aarch64");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::write(orphan_dir.join("bar-1.0-1-aarch64.pkg.tar.zst"), b"x").unwrap();

        let later = Instant::now() + Duration::from_secs(7200);
        assert_eq!(
            repo.sweep_at(later, Duration::from_secs(3600))
                .await
                .unwrap(),
            2,
            "only foo 1.0 and its signature"
        );

        assert!(x86.join(DB_ARCHIVE).exists());
        assert!(x86.join(FILES_ARCHIVE).exists());
        assert!(x86.join("mirrorlist").exists());
        assert!(orphan_dir.join("bar-1.0-1-aarch64.pkg.tar.zst").exists());
    }

    /// A commit that fails for good leaves the repository exactly as it was:
    /// the databases untouched, the upload still staged, nothing left beside.
    #[tokio::test]
    async fn a_failed_commit_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        let db_before = std::fs::read(repo.root().join("x86_64").join(DB_ARCHIVE)).unwrap();

        let (staged, entry) = stage(&repo, 2, "bar", "1.0-1").await;
        let attempts = AtomicU32::new(0);
        let mut update = repo.begin().await;
        update.add(Platform::X86_64, staged.clone(), entry);
        let foo = published(&update, &filename("foo", "1.0-1"));
        update.retire(&foo).unwrap();
        let failed: anyhow::Result<()> = update
            .commit(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("the database said no")
            })
            .await;

        assert!(failed.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), COMMIT_ATTEMPTS);
        assert_eq!(
            std::fs::read(repo.root().join("x86_64").join(DB_ARCHIVE)).unwrap(),
            db_before
        );
        assert!(staged.exists(), "the upload must still be staged");
        assert!(
            repo.root()
                .join("x86_64")
                .join(filename("foo", "1.0-1"))
                .exists()
        );
        assert!(
            !repo
                .root()
                .join("x86_64")
                .join(filename("bar", "1.0-1"))
                .exists()
        );
        assert!(next_files_left(&repo).is_empty());
    }

    /// A commit that fails once is tried again, and the update still lands.
    #[tokio::test]
    async fn a_commit_that_fails_once_is_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        let commits = AtomicU32::new(0);

        let mut update = repo.begin().await;
        update.add(Platform::X86_64, staged, entry);
        update
            .commit(|| async {
                if commits.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("busy");
                }
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(commits.load(Ordering::SeqCst), 2);
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// An update dropped without committing changes nothing, and lets the next
    /// one in.
    #[tokio::test]
    async fn an_update_dropped_uncommitted_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        {
            let mut update = repo.begin().await;
            update.add(Platform::X86_64, staged.clone(), entry.clone());
        }
        assert!(staged.exists());
        assert!(!repo.root().join("x86_64").exists());

        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// A rebuild at the same version removes and adds the same filename: the
    /// new file must be what is left.
    #[tokio::test]
    async fn replacing_a_file_with_one_of_the_same_name_keeps_the_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        for build_id in [1, 2] {
            let (staged, entry) = stage(&repo, build_id, "foo", "1.0-1").await;
            apply(&repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
                .await
                .unwrap();
        }
        assert!(
            repo.root()
                .join("x86_64")
                .join(filename("foo", "1.0-1"))
                .exists()
        );
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// Two updates started together on one platform both land: the second
    /// starts from the databases the first wrote, not from the ones before it.
    #[tokio::test]
    async fn concurrent_updates_do_not_lose_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Arc::new(Repository::new(tmp.path()));
        let (foo_staged, foo) = stage(&repo, 1, "foo", "1.0-1").await;
        let (bar_staged, bar) = stage(&repo, 2, "bar", "1.0-1").await;

        let slow = {
            let repo = Arc::clone(&repo);
            async move {
                let mut update = repo.begin().await;
                update.add(Platform::X86_64, foo_staged, foo);
                // Long enough for the other update to be waiting on it.
                update
                    .commit(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(())
                    })
                    .await
            }
        };
        let fast = {
            let repo = Arc::clone(&repo);
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                apply(&repo, vec![(bar_staged, bar)], &[]).await
            }
        };
        let (a, b) = tokio::join!(slow, fast);
        a.unwrap();
        b.unwrap();

        assert_eq!(listed(&repo), ["bar-1.0-1", "foo-1.0-1"]);
    }
}
