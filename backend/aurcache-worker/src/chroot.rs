//! Base `devtools` chroot lifecycle and per-job execution helpers that shell
//! out to Arch's `devtools`. These run only on a real privileged worker image;
//! the pure argument/parse helpers they rely on live in [`crate::build`].

use anyhow::{Context, Result, bail};
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
/// Only `GNUPGHOME` is preserved: it is the one variable the worker actually
/// exports (see [`import_pgp_keys`]). `SRCDEST`/`PKGDEST` are passed to
/// `makechrootpkg`/`makepkg.conf` by other means, never via the environment,
/// so preserving them here would be a no-op.
pub fn devtools(program: &str) -> Command {
    let mut cmd = Command::new("sudo");
    // `SRCDEST` is how `makechrootpkg` is told where to keep downloaded
    // sources; sudo would otherwise strip it and devtools would silently fall
    // back to the PKGBUILD directory, losing the cache.
    cmd.arg("--preserve-env=GNUPGHOME,SRCDEST").arg(program);
    cmd
}

/// Ensure the shared base chroot exists and is reasonably fresh. Idempotent.
///
/// * Creates `<chroot_dir>/root` via `mkarchroot` seeded with the job's
///   `pacman.conf` on first use. The chroot keeps the `makepkg.conf` its own
///   `pacman` installs; this worker's settings arrive as a drop-in instead.
/// * Otherwise refreshes it with `arch-nspawn … pacman -Syu`.
pub async fn ensure_base_chroot(chroot_dir: &Path, pacman_conf: &Path) -> Result<PathBuf> {
    // Serialize base-chroot creation/refresh: concurrent jobs must not race to
    // build or `-Syu` the same shared root.
    let _guard = BASE_CHROOT_LOCK.lock().await;

    std::fs::create_dir_all(chroot_dir)
        .with_context(|| format!("creating chroot dir {}", chroot_dir.display()))?;
    let root = chroot_dir.join("root");

    if root.join(".arch-chroot").exists() || (root.exists() && root.join("usr").exists()) {
        // Refresh existing chroot; a failure here is non-fatal for the build.
        let mut cmd = devtools("arch-nspawn");
        cmd.arg(&root).args(["pacman", "-Syu", "--noconfirm"]);
        if let Ok((log, status)) = run_capture(cmd).await
            && !status.success()
        {
            tracing::warn!("chroot refresh returned non-zero:\n{log}");
        }
        // Existing chroots too: this arrived after the first ones were built,
        // and a chroot is long-lived.
        ensure_multilib(&root).await;
        return Ok(root);
    }

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
        .arg(&root)
        .arg("base-devel")
        // git+ssh sources are fetched by makepkg *inside* the chroot, and
        // base-devel carries neither git nor an ssh client.
        .arg("git")
        .arg("openssh");
    let (log, status) = run_capture(cmd).await?;
    if !status.success() {
        bail!("mkarchroot failed:\n{log}");
    }
    ensure_multilib(&root).await;
    Ok(root)
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

/// Import the job's trusted PGP keys into a keyring inside the chroot copy's
/// build user. Best-effort: a key server hiccup should not abort the build if
/// the key turns out unnecessary (PKGBUILD `validpgpkeys` still gates trust).
pub async fn import_pgp_keys(gnupg_home: &Path, keyserver: &str, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(gnupg_home)
        .with_context(|| format!("creating gnupg home {}", gnupg_home.display()))?;
    for key in keys {
        let mut cmd = Command::new("gpg");
        cmd.env("GNUPGHOME", gnupg_home).args([
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
    Ok(())
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

pub async fn install_makepkg_dropin(root: &Path, staged: &Path) -> Result<()> {
    let dest = root.join("etc/makepkg.conf.d/aurcache.conf");
    let filtered = without_chroot_owned(
        &std::fs::read_to_string(staged)
            .with_context(|| format!("reading {}", staged.display()))?,
    );
    let staged = &staged.with_extension("dropin");
    std::fs::write(staged, filtered).with_context(|| format!("writing {}", staged.display()))?;
    // Through `sudo`, because the chroot belongs to root and this worker
    // deliberately does not. The file is staged in the job's own directory
    // first, so nothing writes into the chroot except this one copy -- which is
    // the same division `mkarchroot -M` used to provide.
    let mut cmd = Command::new("sudo");
    cmd.arg("install").arg("-Dm644").arg(staged).arg(&dest);
    let (log, status) = run_capture(cmd).await?;
    if !status.success() {
        bail!(
            "installing makepkg overrides into {}:\n{log}",
            dest.display()
        );
    }
    Ok(())
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

/// Whether a name in the chroot directory is a per-build copy.
///
/// `job-<build id>`, and `job-<build id>-<pid>` once devtools has added the
/// suffix `-T` gives it. The copy's lock sits *beside* it rather than inside
/// and is removed along with the copy it belongs to, so matching it here as
/// well reported twice as many reclaimed as there were.
fn is_stale_copy(name: &str) -> bool {
    name.starts_with("job-") && !name.ends_with(".lock")
}

/// Remove one chroot copy, whatever kind of thing it is.
///
/// A copy on btrfs is a subvolume snapshot, and `rm -rf` cannot delete a
/// subvolume -- it empties it and then fails on the directory itself. This is
/// the same distinction `delete_chroot` makes inside devtools.
async fn remove_copy(path: &Path) -> Result<()> {
    let mut show = Command::new("btrfs");
    show.arg("subvolume").arg("show").arg(path);
    // A missing `btrfs` binary means this cannot be a subvolume.
    let is_subvolume = matches!(run_capture(show).await, Ok((_, status)) if status.success());

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
    // devtools keeps the copy's lock beside it, not inside it.
    std::fs::remove_file(path.with_extension("lock")).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
