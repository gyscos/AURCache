//! Base `devtools` chroot lifecycle and per-job execution helpers that shell
//! out to Arch's `devtools`. These run only on a real privileged worker image;
//! the pure argument/parse helpers they rely on live in [`crate::build`].

use anyhow::{Context, Result, bail};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::report_warning;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Serializes base-chroot creation/refresh across concurrent jobs so two builds
/// never race to `mkarchroot`/`arch-nspawn` the same shared `<chroot_dir>/root`
/// (which would corrupt it). A worker only ever uses one chroot dir, so a
/// single process-wide lock is enough.
static BASE_CHROOT_LOCK: Mutex<()> = Mutex::const_new(());

/// Run a command, returning combined stdout+stderr and the exit status.
pub async fn run_capture(mut cmd: Command) -> Result<(String, std::process::ExitStatus)> {
    let output = cmd.output().await.context("spawning command")?;
    let mut log = String::new();
    log.push_str(&String::from_utf8_lossy(&output.stdout));
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((log, output.status))
}

/// Build a `Command` that runs a privileged `devtools` program via `sudo`.
///
/// The worker runs as the unprivileged `aurcache` user; `sudo` provides the
/// root that mounting, unsharing and chrooting need. Which user the *build*
/// runs as is passed explicitly with `-U` rather than left to `makechrootpkg`'s
/// `SUDO_USER` inference, so it is `builder` and never `aurcache` -- see
/// `build::build_command`.
///
/// Four variables are preserved, because sudo would otherwise drop all of
/// them: `GNUPGHOME` names this build's keyring replica (see
/// [`prepare_job_keyring`]), `SRCDEST` the source cache `makechrootpkg` binds
/// itself, `AURCACHE_DROPIN` the makepkg overrides it installs into this
/// build's chroot copy, and `AURCACHE_NSPAWN_KEEP_UNIT` whether the container
/// stays in the build's cgroup. `PKGDEST` reaches the build through
/// `makepkg.conf` instead, so preserving it here would be a no-op.
pub fn devtools(program: &str) -> Command {
    let mut cmd = Command::new("sudo");
    // `SRCDEST` is how `makechrootpkg` is told where to keep downloaded
    // sources; sudo would otherwise strip it and devtools would silently fall
    // back to the PKGBUILD directory, losing the cache.
    cmd.arg("--preserve-env=GNUPGHOME,SRCDEST,AURCACHE_DROPIN,AURCACHE_NSPAWN_KEEP_UNIT")
        .arg(program);
    cmd
}

/// Ensure the shared base chroot exists and is reasonably fresh. Idempotent.
///
/// * Creates `<chroot_dir>/root` via `mkarchroot` seeded with the job's
///   `pacman.conf` on first use. The chroot keeps the `makepkg.conf` its own
///   `pacman` installs; this worker's settings arrive as a drop-in instead.
/// * Otherwise refreshes it with `arch-nspawn … pacman -Syu`.
///
/// **The caller must hold `root.lock`** while this runs, or hold nothing
/// because there was no lock to hold. `makechrootpkg` takes it shared across
/// `sync_chroot` so it never clones a half-updated chroot, and an overlay
/// build holds it shared for its whole life because its lower layer must not
/// change -- but `arch-nspawn`, which is how the chroot gets updated, takes no
/// lock at all, so devtools' care only ever covered devtools' own writers.
/// [`crate::chroots::Chroots::refresh`] is what decides that here.
///
/// `report_to` names who to tell about a refresh problem, and the build to
/// file it under -- `None` for `build-once`, which runs this same machinery
/// with no server and no build id to name.
pub async fn ensure_base_chroot(
    chroot_dir: &Path,
    pacman_conf: &Path,
    report_to: Option<(&WorkerClient, i32)>,
) -> Result<PathBuf> {
    // Serialize base-chroot creation/refresh: concurrent jobs must not race to
    // build or `-Syu` the same shared root.
    let _guard = BASE_CHROOT_LOCK.lock().await;

    let root = base_dir(chroot_dir)?;
    if base_exists(&root) {
        // Refresh existing chroot; a failure here is non-fatal for the build.
        let mut cmd = devtools("arch-nspawn");
        cmd.arg(&root).args(["pacman", "-Syu", "--noconfirm"]);
        if let Ok((log, status)) = run_capture(cmd).await
            && !status.success()
        {
            tracing::warn!("chroot refresh returned non-zero:\n{log}");
            if let Some((client, build_id)) = report_to {
                report_warning(
                    client,
                    Some(build_id),
                    &format!("chroot refresh returned non-zero:\n{log}"),
                )
                .await;
            }
        }
        // Existing chroots too: this arrived after the first ones were built,
        // and a chroot is long-lived.
        ensure_multilib(&root).await;
        return Ok(root);
    }
    create_base(&root, pacman_conf).await
}

/// Create the base chroot if it is missing, and leave it alone if it is not.
///
/// What an overlay worker calls instead of [`ensure_base_chroot`]: its base is
/// frozen, and a refresh there is a new layer on top rather than a change to
/// the thing every running build is reading. See `design/overlay-chroot.md`.
pub async fn create_base_chroot(chroot_dir: &Path, pacman_conf: &Path) -> Result<PathBuf> {
    let _guard = BASE_CHROOT_LOCK.lock().await;

    let root = base_dir(chroot_dir)?;
    if base_exists(&root) {
        return Ok(root);
    }
    create_base(&root, pacman_conf).await
}

/// The base chroot's path, with its directory made.
fn base_dir(chroot_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(chroot_dir)
        .with_context(|| format!("creating chroot dir {}", chroot_dir.display()))?;
    Ok(chroot_dir.join("root"))
}

/// Whether there is a chroot here already, rather than an empty directory.
fn base_exists(root: &Path) -> bool {
    root.join(".arch-chroot").exists() || (root.exists() && root.join("usr").exists())
}

/// `mkarchroot` a fresh base chroot.
async fn create_base(root: &Path, pacman_conf: &Path) -> Result<PathBuf> {
    // No `-M`. `mkarchroot` would copy a makepkg.conf into the chroot, and its
    // default is the *host's* -- which is the operator's own build
    // configuration, not this service's. A developer machine with
    // `BUILDENV=(... ccache ...)` is entirely ordinary, and carrying that into a
    // clean `base-devel` chroot fails every build before it compiles anything:
    //
    //   ==> ERROR: Cannot find the ccache binary required for compiler cache usage.
    //
    // Left alone, the chroot keeps the `makepkg.conf` its own `pacman` package
    // installed: complete, correct for the chroot's architecture, and owing
    // nothing to whatever the host happens to be configured for. What this
    // worker wants to change goes in `makepkg.conf.d/` instead.
    let mut cmd = devtools("mkarchroot");
    cmd.arg("-C")
        .arg(pacman_conf)
        .arg(root)
        .arg("base-devel")
        // git+ssh sources are fetched by makepkg *inside* the chroot, and
        // base-devel carries neither git nor an ssh client.
        .arg("git")
        .arg("openssh");
    let (log, status) = run_capture(cmd).await?;
    if !status.success() {
        bail!("mkarchroot failed:\n{log}");
    }
    ensure_multilib(root).await;
    Ok(root.to_path_buf())
}

/// Install the 32-bit toolchain, where the architecture has one.
///
/// `base-devel` builds 64-bit only, so every `lib32-*` package -- which
/// compiles with `-m32` -- failed at configure with the singularly unhelpful
/// "C compiler cannot create executables". Sixteen of them did before this
/// existed, while the packages that happened to declare `gcc-multilib` among
/// their `makedepends` succeeded, which made it look like an intermittent
/// problem with particular packages rather than a missing toolchain.
///
/// **In a transaction of its own, and never folded into `mkarchroot` or the
/// `-Syu`.** pacman aborts an entire transaction when a single target cannot be
/// resolved, so asking for this alongside `base-devel` would mean an
/// architecture without multilib -- every one except x86_64 -- silently getting
/// no toolchain at all rather than merely no 32-bit one. The container builder
/// learned this the expensive way; `docker/add-aur.sh` carries the scar.
///
/// Best-effort for the same reason: absence is the expected case off x86_64,
/// not a failure worth refusing to build over.
async fn ensure_multilib(root: &Path) {
    let mut cmd = devtools("arch-nspawn");
    cmd.arg(root).args([
        "pacman",
        "-S",
        "--needed",
        "--noconfirm",
        "--noprogressbar",
        "multilib-devel",
    ]);
    match run_capture(cmd).await {
        Ok((_, status)) if status.success() => {}
        Ok((log, _)) => tracing::debug!(
            "multilib-devel not installed (expected off x86_64); \
             lib32-* packages will not build here:\n{log}"
        ),
        Err(e) => tracing::warn!("could not check for multilib-devel: {e:#}"),
    }
}

/// Serializes every write to the shared keyring, and every copy taken from it.
///
/// The worker is that keyring's only writer, but it runs jobs concurrently, so
/// two `--recv-keys` children would otherwise race — and worse, a replica could
/// be copied out of a keybox mid-rewrite. Held across the fetch as well as the
/// copy: a key already present is not fetched again, so once warm this is a
/// file copy's worth of contention at the start of a build.
static KEYRING_LOCK: Mutex<()> = Mutex::const_new(());

/// What a read-only GnuPG home needs to be usable.
///
/// gpg takes a dotlock in its home even to *read* a keybox, and rewrites the
/// trustdb it is handed; the build user can write neither, here or anywhere
/// else outside `SRCDEST`/`BUILDDIR` (`aurcache-sandbox` enforces that with
/// Landlock). Locking is safe to drop precisely because the replica has no
/// writer for the life of the build -- which is the whole reason it is a
/// replica.
const REPLICA_GPG_CONF: &str = "lock-never\nno-auto-check-trustdb\n";

/// Files that make up the replica. `trustdb.gpg` is not optional: gpg aborts
/// with `Fatal: can't create trustdb.gpg` on a read-only home that has a
/// keyring but no trustdb, before it reports anything about the signature.
const REPLICA_FILES: [&str; 2] = ["pubring.kbx", "trustdb.gpg"];

/// Stage the job's trusted PGP keys into a keyring of this build's own.
///
/// Keys are fetched once into `shared` and kept there, so a key some earlier
/// build already needed costs no keyserver round trip. What the build reads is
/// `replica`, a copy of that keyring: `makechrootpkg` verifies source
/// signatures *on the worker*, as the build user, while sibling jobs may be
/// importing keys — and a keybox rewritten under a reader fails the
/// verification. gpg's own dotlock does not prevent that; a reader still lands
/// between the writer's rename steps and reports `keydb_search failed: No such
/// file or directory`, measured at roughly one read in fifty during an import,
/// with locking on or off. A replica has no writer at all.
///
/// Best-effort: a key server hiccup should not abort the build if the key turns
/// out unnecessary (PKGBUILD `validpgpkeys` still gates trust).
pub async fn prepare_job_keyring(
    shared: &Path,
    replica: &Path,
    keyserver: &str,
    keys: &[String],
) -> Result<()> {
    let _guard = KEYRING_LOCK.lock().await;
    std::fs::create_dir_all(shared)
        .with_context(|| format!("creating gnupg home {}", shared.display()))?;
    for key in keys {
        if has_key(shared, key).await {
            tracing::debug!("pgp key {key} already in the shared keyring");
            continue;
        }
        let mut cmd = Command::new("gpg");
        cmd.env("GNUPGHOME", shared).args([
            "--batch",
            "--keyserver",
            keyserver,
            "--recv-keys",
            key,
        ]);
        match run_capture(cmd).await {
            Ok((_, status)) if status.success() => {
                tracing::info!("imported pgp key {key}");
            }
            Ok((log, _)) => tracing::warn!("could not import pgp key {key}:\n{log}"),
            Err(e) => tracing::warn!("gpg failed for key {key}: {e}"),
        }
    }
    // Ensure the trustdb exists before it is copied, rather than leaving the
    // build to discover it cannot create one. Idempotent and near-instant on a
    // keyring this size ("no need for a trustdb check" once current).
    let mut cmd = Command::new("gpg");
    cmd.env("GNUPGHOME", shared)
        .args(["--batch", "--check-trustdb"]);
    if let Ok((log, status)) = run_capture(cmd).await
        && !status.success()
    {
        tracing::warn!("could not prepare the shared trustdb:\n{log}");
    }
    // Always staged, even for a package that declares no keys: a PKGBUILD with
    // signed sources and no `validpgpkeys` then fails with "unknown public
    // key", which says what is wrong, instead of gpg dying on a home it cannot
    // write and makepkg reporting "SIGNATURE NOT FOUND".
    copy_keyring(shared, replica)
}

/// Whether the shared keyring already holds a key, so it is not fetched twice.
async fn has_key(gnupg_home: &Path, key: &str) -> bool {
    let mut cmd = Command::new("gpg");
    cmd.env("GNUPGHOME", gnupg_home)
        .args(["--batch", "--list-keys", key]);
    matches!(run_capture(cmd).await, Ok((_, status)) if status.success())
}

/// Copy the shared keyring into a home the build user can read and nothing can
/// write.
///
/// The modes are set explicitly because `copy` carries the source's across:
/// gpg creates `trustdb.gpg` `0600`, and a replica the build cannot read fails
/// exactly like no replica at all.
fn copy_keyring(shared: &Path, replica: &Path) -> Result<()> {
    std::fs::create_dir_all(replica)
        .with_context(|| format!("creating job keyring {}", replica.display()))?;
    for name in REPLICA_FILES {
        let from = shared.join(name);
        if !from.exists() {
            continue;
        }
        let to = replica.join(name);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        set_readable(&to);
    }
    let conf = replica.join("gpg.conf");
    std::fs::write(&conf, REPLICA_GPG_CONF)
        .with_context(|| format!("writing {}", conf.display()))?;
    set_readable(&conf);
    Ok(())
}

/// Make a replica file readable by the build user, which is not the user that
/// wrote it.
fn set_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)) {
            tracing::warn!("could not make {} readable: {e}", path.display());
        }
    }
}

/// Install the job's makepkg overrides into the chroot's `makepkg.conf.d/`.
///
/// `makepkg` sources `$MAKEPKG_CONF` and then every `$MAKEPKG_CONF.d/*.conf`
/// (see `source_makepkg_config` in `/usr/share/makepkg/util/config.sh`), so a
/// drop-in wins over the base without replacing it. That is what lets the
/// chroot keep its own complete, architecture-correct defaults while this
/// worker still forces `PKGDEST`, `MAKEFLAGS`, `PACKAGER` and the operator's
/// own `makepkg_conf` setting.
///
/// Written to the *base* chroot before each build: `makechrootpkg` copies the
/// base into a per-job chroot when it runs, so the drop-in travels with it.
/// Rewriting it per build is also what makes a per-package `makepkg_conf`
/// setting take effect at all -- the previous arrangement only reached the
/// chroot when it was first created, so a setting changed afterwards was
/// silently ignored until the chroot was rebuilt.
/// Settings `makechrootpkg` chooses for itself, which a drop-in must not touch.
///
/// It appends `BUILDDIR=/build PKGDEST=/pkgdest ...` to the chroot's
/// `/etc/makepkg.conf`, skipping any it finds already set. Those paths are
/// bind-mounted into the build; ours are not. While the server's `PKGDEST` sat
/// in that same file, makechrootpkg's line was appended after it and won -- so
/// it was always a no-op for a chroot build. From a drop-in, which makepkg
/// sources *after* the base file, it would win instead, and the build fails
/// immediately:
///
/// ```text
/// ==> ERROR: Failed to create the directory $PKGDEST (/output).
/// ```
const CHROOT_OWNED_SETTINGS: [&str; 5] =
    ["BUILDDIR", "PKGDEST", "SRCPKGDEST", "SRCDEST", "LOGDEST"];

/// Drop assignments to the settings `makechrootpkg` owns.
fn without_chroot_owned(overrides: &str) -> String {
    overrides
        .lines()
        .filter(|line| {
            let key = line.trim_start().split('=').next().unwrap_or("").trim();
            !CHROOT_OWNED_SETTINGS.contains(&key)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Write this build's makepkg overrides to a file of its own, and return it.
///
/// `makechrootpkg` installs it into the job's chroot copy (see
/// `packaging/patch-makechrootpkg.py`), which is why nothing here touches the
/// base chroot. It used to: the drop-in went into the shared template before
/// every build, and `sync_chroot` carried it into the copy -- so two builds
/// starting together raced, and one could run with the other's `MAKEFLAGS`,
/// `PACKAGER` and ssh-agent socket. The copy is per build by construction,
/// and the worker cannot see when `makechrootpkg` makes it, so there is no
/// lock that would have fixed this.
pub fn stage_makepkg_dropin(staged: &Path) -> Result<PathBuf> {
    let filtered = without_chroot_owned(
        &std::fs::read_to_string(staged)
            .with_context(|| format!("reading {}", staged.display()))?,
    );
    let dropin = staged.with_extension("dropin");
    std::fs::write(&dropin, filtered).with_context(|| format!("writing {}", dropin.display()))?;
    Ok(dropin)
}

/// Append the worker's cache layout to a server-rendered `pacman.conf`.
///
/// `arch-nspawn` reads `CacheDir` from the chroot's own `pacman.conf` and
/// bind-mounts the **first** entry read-write into the container, the rest
/// read-only. Listing a private job directory first and the shared cache second
/// gives each build somewhere private to download to while still reading hits
/// from the shared pool.
///
/// The first path is a fixed mount point rather than the real per-job
/// directory: the build's `pacman.conf` is inherited from the base chroot and
/// is therefore identical for every job, so the per-job part is supplied as a
/// bind mount over that path instead (see `job.rs`).
///
/// Written worker-side rather than by the server because cache paths are
/// worker-local — the same reasoning that keeps `GIT_SSH_COMMAND` out of the
/// job descriptor.
fn with_cache_dirs(pacman_conf: &str, shared_pkg_cache: Option<&Path>) -> String {
    let Some(shared) = shared_pkg_cache else {
        return pacman_conf.to_string();
    };
    let mut out = pacman_conf.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Added by aurcache-worker: per-job writable cache first (bound over by\n");
    out.push_str("# the job's private directory), shared read-only cache second.\n");
    let _ = write!(
        out,
        "[options]\nCacheDir = {PER_JOB_CACHE_MOUNT} {}\n",
        shared.display()
    );
    out
}

/// Mount point a job's private pacman cache is bound over. Matches pacman's
/// default so nothing else has to change.
/// Where `makechrootpkg` points `BUILDDIR` inside the chroot. A persistent
/// build tree is bound here, so makepkg's own layout is unchanged and only the
/// lifetime differs.
pub const BUILDDIR_MOUNT: &str = "/build";

pub const PER_JOB_CACHE_MOUNT: &str = "/var/cache/pacman/pkg";

/// Write the per-package makepkg overrides, `pacman.conf` and mirrorlist to a
/// staging directory the caller seeds the base chroot from.
///
/// The makepkg file here is *only* the overrides, not a whole `makepkg.conf`:
/// [`install_makepkg_dropin`] puts it in the chroot's `makepkg.conf.d/`, so the
/// chroot keeps its own defaults rather than having them replaced.
pub fn write_configs(
    dir: &Path,
    makepkg_conf: &str,
    pacman_conf: &str,
    mirrorlist: Option<&str>,
    shared_pkg_cache: Option<&Path>,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    let makepkg = dir.join("makepkg.conf");
    let pacman = dir.join("pacman.conf");

    std::fs::write(&makepkg, makepkg_conf).context("writing makepkg overrides")?;
    std::fs::write(&pacman, with_cache_dirs(pacman_conf, shared_pkg_cache))
        .context("writing pacman.conf")?;
    if let Some(list) = mirrorlist {
        std::fs::write(dir.join("mirrorlist"), list).context("writing mirrorlist")?;
    }
    Ok((makepkg, pacman))
}

/// Delete per-build chroot copies left behind by earlier runs.
///
/// Each build's copy is temporary now (`makechrootpkg -T`), so in the ordinary
/// case there is nothing here to find. A worker that was killed mid-build
/// leaves one anyway, and a worker upgraded from a version that never passed
/// `-T` leaves every copy it ever made -- which is how this host reached 26 of
/// them and 362G before the disk filled.
///
/// Startup is the safe moment: this worker is running no builds yet, so every
/// `job-*` under the chroot directory is by definition garbage. It is *not*
/// safe to run later, and two workers must not share a chroot directory.
///
/// Never fatal. Failing to reclaim space is worth reporting and carrying on;
/// refusing to start over it would turn a full disk into an offline worker.
pub async fn remove_stale_copies(chroot_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(chroot_dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !is_stale_copy(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        match remove_copy(&path).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!("could not remove stale chroot {}: {e:#}", path.display()),
        }
    }
    if removed > 0 {
        tracing::info!("removed {removed} stale chroot cop(ies) from a previous run");
    }
    removed
}

/// What asking for the base chroot's lock produced.
pub enum BaseLock {
    /// Held. The base chroot is ours to change until this is dropped.
    Held(std::fs::File),
    /// Someone else holds it: a copy being taken, or an overlay build using
    /// the base as its lower layer.
    Busy,
    /// There is no usable lock here at all. Not a reason to refuse to work --
    /// that is what happened before any of this existed.
    Unavailable,
}

/// Ask for devtools' `root.lock` exclusively, without waiting.
///
/// Never blocks, and that is the point. An overlay build holds the lock
/// *shared* for its whole life, so a blocking wait here would hold up every
/// job start behind it for as long as the longest build runs. A refresh that
/// cannot have the chroot right now simply happens later; being a few minutes
/// out of date is a smaller problem than a worker that starts no builds.
pub async fn try_lock_base(root: &Path) -> BaseLock {
    // Same rule as every other lock path here (`lock_beside`): append, never
    // `with_extension`, which would replace an extension instead. All three
    // must name the same file or the exclusion silently stops excluding.
    let Some(path) = lock_beside(root) else {
        tracing::warn!("could not form a lock path for {}", root.display());
        return BaseLock::Unavailable;
    };
    let taken = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    })
    .await;
    match taken {
        Ok(Ok(Some(file))) => BaseLock::Held(file),
        Ok(Ok(None)) => BaseLock::Busy,
        Ok(Err(e)) => {
            tracing::warn!("could not lock the base chroot: {e}");
            BaseLock::Unavailable
        }
        Err(e) => {
            tracing::warn!("could not lock the base chroot: {e}");
            BaseLock::Unavailable
        }
    }
}

/// Take devtools' `root.lock` *shared*, which is what `sync_chroot` does while
/// it copies.
///
/// An overlay uses the base chroot as its lower layer for the whole build, not
/// for the instant of a copy, so it holds the same shared lock for the whole
/// build: several may read it at once, and a refresh -- which takes it
/// exclusively -- waits for them.
pub async fn share_base_chroot(root: &Path) -> Option<std::fs::File> {
    open_base_lock(root).await
}

/// Open and lock the base chroot's lock file.
///
/// Opened **read-only**, which is not a detail: `mkarchroot` creates the lock
/// as root and leaves it `0644`, while the worker is not root, so asking for
/// write access fails with `EACCES` and the lock is never taken -- silently,
/// since this is best-effort. `flock(2)` places either kind of lock through a
/// read-only descriptor perfectly well; the open mode and the lock mode are
/// unrelated. There is nothing to create here either: by the time a chroot can
/// be refreshed or overlaid, `mkarchroot` has made both it and its lock.
///
/// The lock is held until the returned file is dropped. `None` if it could not
/// be taken at all, which is worth carrying on without -- that is what
/// happened before this existed, and refusing to build over it would be a
/// worse trade.
async fn open_base_lock(root: &Path) -> Option<std::fs::File> {
    // See `try_lock_base`: one rule for every lock path.
    let path = lock_beside(root)?;
    let taken = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        file.lock_shared()?;
        std::io::Result::Ok(file)
    })
    .await;
    match taken {
        Ok(Ok(file)) => Some(file),
        Ok(Err(e)) => {
            tracing::warn!("could not lock the base chroot: {e}");
            None
        }
        Err(e) => {
            tracing::warn!("could not lock the base chroot: {e}");
            None
        }
    }
}

/// Whether a name in the chroot directory is a per-build copy.
///
/// `job-<build id>`, and `job-<build id>-<pid>` once devtools has added the
/// suffix `-T` gives it. The copy's lock sits *beside* it rather than inside
/// and is removed along with the copy it belongs to, so matching it here as
/// well reported twice as many reclaimed as there were.
fn is_stale_copy(name: &str) -> bool {
    name.starts_with("job-") && !name.ends_with(".lock")
}

/// Whether `name` is the copy `makechrootpkg -l <label> -T` made: `-T` appends
/// `-$$`, so `<label>-<pid>` and nothing else. Exact, because labels share
/// prefixes -- `job-99`'s copy must never match `job-995-482762`.
fn is_copy_of(name: &str, label: &str) -> bool {
    name.strip_prefix(label)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Delete the copy one build's `makechrootpkg` left behind, if it left one.
///
/// It deletes its own copy on the way out -- from an EXIT trap, which a SIGKILL
/// never runs. Stopping a build kills its whole cgroup, so every stopped build
/// used to leave a full chroot copy (4.4G for unreal-engine) on disk until the
/// worker next restarted and swept `job-*`. Called once the build's tree is
/// dead, and matching only this build's label, so it cannot reach a copy
/// another build is using. A build that ended normally has nothing here, and
/// this costs one `read_dir`.
pub async fn remove_leftover_copy(chroot_dir: &Path, label: &str) {
    let Ok(entries) = std::fs::read_dir(chroot_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_copy_of(&entry.file_name().to_string_lossy(), label) {
            continue;
        }
        let path = entry.path();
        match remove_copy(&path).await {
            Ok(()) => tracing::info!(
                "removed chroot copy {} left by a killed build",
                path.display()
            ),
            Err(e) => tracing::warn!("could not remove chroot copy {}: {e:#}", path.display()),
        }
    }
}

/// The lock `makechrootpkg` takes for a copy, which sits beside it rather than
/// inside it.
fn lock_beside(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(path.with_file_name(format!("{name}.lock")))
}

/// btrfs, from `statfs`.
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

/// Every btrfs subvolume root has this inode number, and only a subvolume root
/// has it -- on btrfs. Any other filesystem hands out 256 like any other
/// number, which is why the filesystem type is half the test.
const BTRFS_SUBVOLUME_INODE: u64 = 256;

/// Whether a filesystem type and inode number describe a subvolume root.
fn is_subvolume_ino(fs_type: i64, inode: u64) -> bool {
    fs_type == BTRFS_SUPER_MAGIC && inode == BTRFS_SUBVOLUME_INODE
}

/// Whether a path is a btrfs subvolume, by the test devtools uses.
///
/// Deliberately not `btrfs subvolume show`: that searches the B-tree and needs
/// root, so asking it as the worker's own user answers "not a subvolume" for
/// every subvolume there is. The caller then reaches for `rm`, which cannot
/// delete a subvolume -- and every copy this ever swept was left on disk, one
/// per crashed build, forever. `statfs` and the inode number need no
/// privileges.
fn is_btrfs_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Some(fs_type) = fs_type(path) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    is_subvolume_ino(fs_type, meta.ino())
}

/// The filesystem type under `path`, from `statfs`.
fn fs_type(path: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated path and `buf` is a valid statfs.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    Some(buf.f_type as i64)
}

/// Whether `path` is on btrfs, where `makechrootpkg` copies a chroot by taking
/// a snapshot and there is nothing for an overlay to save.
#[must_use]
pub fn is_btrfs(path: &Path) -> bool {
    fs_type(path) == Some(BTRFS_SUPER_MAGIC)
}

/// Remove one chroot copy, whatever kind of thing it is.
///
/// A copy on btrfs is a subvolume snapshot, and `rm -rf` cannot delete a
/// subvolume -- it empties it and then fails on the directory itself. This is
/// the same distinction `delete_chroot` makes inside devtools.
async fn remove_copy(path: &Path) -> Result<()> {
    let is_subvolume = is_btrfs_subvolume(path);

    let mut cmd = Command::new("sudo");
    if is_subvolume {
        cmd.arg("btrfs").arg("subvolume").arg("delete").arg(path);
    } else {
        // `--one-file-system`, as devtools does: an unmount that failed leaves
        // something mounted under here, and this must not walk into it.
        cmd.arg("rm")
            .arg("--recursive")
            .arg("--force")
            .arg("--one-file-system")
            .arg(path);
    }
    let (log, status) = run_capture(cmd).await?;
    if !status.success() {
        bail!("removing {}:\n{log}", path.display());
    }
    // The lock lives beside the copy and is root's, like the copy. devtools
    // removes it in `delete_chroot`; a sweep that does not leaves one empty
    // file per crashed build behind for good.
    if let Some(lock) = lock_beside(path) {
        let mut cmd = Command::new("sudo");
        cmd.arg("rm").arg("--force").arg(&lock);
        if let Ok((log, status)) = run_capture(cmd).await
            && !status.success()
        {
            tracing::warn!("could not remove {}:\n{log}", lock.display());
        }
    }
    // The lock beside the copy went out with the `sudo rm` above, which names
    // the same `lock_beside` path: devtools keeps it beside the copy, not
    // inside it, and it is root's. (A second computation of the path used to
    // live here with `with_extension` instead of the append rule — same file
    // for every real copy name, a different one the moment a dot appears.)
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The replica has to carry a trustdb, not just a keyring. On a home it
    /// cannot write, gpg dies with `Fatal: can't create trustdb.gpg` before it
    /// says anything about the signature -- and makepkg reports that silence
    /// as "SIGNATURE NOT FOUND".
    #[test]
    fn the_replica_carries_a_trustdb() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("pubring.kbx"), b"keys").unwrap();
        std::fs::write(shared.join("trustdb.gpg"), b"trust").unwrap();

        copy_keyring(&shared, &replica).unwrap();

        for name in REPLICA_FILES {
            assert!(replica.join(name).exists(), "{name} must reach the replica");
        }
    }

    /// gpg creates `trustdb.gpg` `0600`, and `copy` carries the source's mode
    /// across -- so an unadjusted replica is unreadable by the build user,
    /// which fails exactly like having no replica at all.
    #[test]
    fn the_replica_is_readable_by_another_user() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("trustdb.gpg"), b"trust").unwrap();
        std::fs::set_permissions(
            shared.join("trustdb.gpg"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        copy_keyring(&shared, &replica).unwrap();

        for name in ["trustdb.gpg", "gpg.conf"] {
            let mode = std::fs::metadata(replica.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o044, 0o044, "{name} must be readable by the build");
        }
    }

    /// Without `lock-never` gpg takes a dotlock in its home to *read* the
    /// keybox, which a read-only replica cannot grant; without
    /// `no-auto-check-trustdb` it wants to rewrite the trustdb it was handed.
    #[test]
    fn the_replica_tells_gpg_not_to_write() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();

        copy_keyring(&shared, &replica).unwrap();

        let conf = std::fs::read_to_string(replica.join("gpg.conf")).unwrap();
        assert!(conf.contains("lock-never"), "{conf}");
        assert!(conf.contains("no-auto-check-trustdb"), "{conf}");
    }

    /// The inode number alone means nothing off btrfs, where 256 is just a
    /// number -- and the filesystem type alone means nothing either, since
    /// every directory in a chroot copy sits on btrfs too.
    #[test]
    fn a_subvolume_is_a_filesystem_and_an_inode() {
        assert!(is_subvolume_ino(BTRFS_SUPER_MAGIC, BTRFS_SUBVOLUME_INODE));
        assert!(!is_subvolume_ino(BTRFS_SUPER_MAGIC, 257));
        // ext4.
        assert!(!is_subvolume_ino(0xEF53, BTRFS_SUBVOLUME_INODE));
    }

    /// The lock is a sibling, not a child: `job-604-2844759.lock` beside
    /// `job-604-2844759`.
    #[test]
    fn the_lock_sits_beside_the_copy() {
        assert_eq!(
            lock_beside(Path::new("/chroot/job-604-2844759")),
            Some(PathBuf::from("/chroot/job-604-2844759.lock"))
        );
    }

    /// The lock beside a copy is removed with it, so matching it separately
    /// double-counts what was reclaimed. The base chroot must never match.
    #[test]
    fn only_per_build_copies_are_swept() {
        assert!(is_stale_copy("job-604"));
        assert!(is_stale_copy("job-604-2844759"));
        assert!(!is_stale_copy("job-604-2844759.lock"));
        assert!(!is_stale_copy("root"));
        assert!(!is_stale_copy("root.lock"));
    }

    /// A killed build's copy is found by its own label only. Labels share
    /// prefixes, so a looser match would delete a copy a running build is in.
    #[test]
    fn a_leftover_copy_is_matched_by_its_own_label_only() {
        assert!(is_copy_of("job-995-482762", "job-995"));
        assert!(!is_copy_of("job-995-482762", "job-99"));
        assert!(!is_copy_of("job-995-482762.lock", "job-995"));
        assert!(!is_copy_of("job-995", "job-995"));
        assert!(!is_copy_of("job-995-", "job-995"));
        assert!(!is_copy_of("job-995-abc", "job-995"));
        assert!(!is_copy_of("root", "job-995"));
    }

    /// One build's overrides go to a file of that build's own, which
    /// `makechrootpkg` then installs into the chroot copy it makes. Nothing is
    /// written to the shared base chroot, where two builds starting together
    /// used to overwrite each other's.
    #[test]
    fn a_dropin_is_staged_per_build() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("makepkg.conf");
        std::fs::write(&staged, "MAKEFLAGS=-j4\nPKGDEST=/output\nPACKAGER='a'\n").unwrap();

        let dropin = stage_makepkg_dropin(&staged).unwrap();

        assert_ne!(dropin, staged, "the drop-in is its own file");
        let text = std::fs::read_to_string(&dropin).unwrap();
        assert_eq!(text, "MAKEFLAGS=-j4\nPACKAGER='a'\n");
    }

    /// The variable has to be spelled the same here and in
    /// `packaging/patch-makechrootpkg.py`, which reads `$AURCACHE_DROPIN`:
    /// sudo drops everything not named, and a dropped drop-in is a build that
    /// silently runs on the chroot's own defaults.
    #[test]
    fn the_build_environment_survives_sudo() {
        let cmd = devtools("makechrootpkg");
        let preserve = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .find(|a| a.starts_with("--preserve-env="))
            .expect("devtools commands must preserve an environment through sudo");

        for var in ["GNUPGHOME", "SRCDEST", "AURCACHE_DROPIN"] {
            assert!(preserve.contains(var), "{preserve} must carry {var}");
        }
    }

    /// `makechrootpkg` picks the build directories, and a drop-in must not
    /// override them: it bind-mounts `/pkgdest` and friends, and `/output` --
    /// which the server sends as a default -- exists nowhere in the chroot.
    #[test]
    fn the_chroots_own_directories_are_left_alone() {
        let filtered = without_chroot_owned(
            "PKGDEST=/output\nMAKEFLAGS=-j4\nSRCDEST=/srcdest\nPACKAGER='x'\nBUILDDIR=/b\n",
        );
        assert_eq!(filtered, "MAKEFLAGS=-j4\nPACKAGER='x'\n");
    }

    /// Only exact assignments go; a comment mentioning one stays, and so does
    /// anything whose name merely starts the same way.
    #[test]
    fn filtering_matches_the_setting_not_the_text() {
        let filtered = without_chroot_owned(
            "# PKGDEST is chosen by makechrootpkg\nPKGDEST_EXTRA=1\nPKGDEST=/output\n",
        );
        assert_eq!(
            filtered,
            "# PKGDEST is chosen by makechrootpkg\nPKGDEST_EXTRA=1\n"
        );
    }

    /// The staged makepkg file is the job's overrides verbatim.
    ///
    /// Nothing from the host is mixed in: that is the point of the drop-in, and
    /// a merged file here would mean the chroot's own defaults had been replaced
    /// by whatever the build machine happens to be configured for.
    ///
    /// Installing it into the chroot needs root and is left to the real build.
    #[test]
    fn only_the_jobs_overrides_are_staged() {
        let dir = tempfile::tempdir().unwrap();
        let (makepkg, _pacman) = write_configs(
            dir.path(),
            "PKGDEST=/output\nMAKEFLAGS=-j4\n",
            "[options]\n",
            None,
            None,
        )
        .unwrap();

        let staged = std::fs::read_to_string(&makepkg).unwrap();
        assert_eq!(staged, "PKGDEST=/output\nMAKEFLAGS=-j4\n");
        assert!(
            !staged.contains("CARCH") && !staged.contains("BUILDENV"),
            "the host's defaults must not be merged in: {staged}"
        );
    }
}
