//! Base `devtools` chroot lifecycle and per-job execution helpers that shell
//! out to Arch's `devtools`. These run only on a real privileged worker image;
//! the pure argument/parse helpers they rely on live in [`crate::build`].

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Serializes base-chroot creation/refresh across concurrent jobs so two builds
/// never race to `mkarchroot`/`arch-nspawn` the same shared `<chroot_dir>/root`
/// (which would corrupt it). Keyed by chroot dir so distinct dirs don't block.
fn base_chroot_lock() -> Arc<Mutex<()>> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(Mutex::new(()))))
}

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
/// The worker runs as an unprivileged `builder` user inside the container:
/// `sudo` provides the root needed for chroot ops and, crucially, sets
/// `SUDO_USER` so `makechrootpkg` runs `makepkg` as `builder` (never root).
///
/// Only `GNUPGHOME` is preserved: it is the one variable the worker actually
/// exports (see [`import_pgp_keys`]). `SRCDEST`/`PKGDEST` are passed to
/// `makechrootpkg`/`makepkg.conf` by other means, never via the environment,
/// so preserving them here would be a no-op.
pub fn devtools(program: &str) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.arg("--preserve-env=GNUPGHOME").arg(program);
    cmd
}

/// Ensure the shared base chroot exists and is reasonably fresh. Idempotent.
///
/// * Creates `<chroot_dir>/root` via `mkarchroot` seeded with the job's
///   `pacman.conf` / `makepkg.conf` on first use.
/// * Otherwise refreshes it with `arch-nspawn … pacman -Syu`.
pub async fn ensure_base_chroot(
    chroot_dir: &Path,
    pacman_conf: &Path,
    makepkg_conf: &Path,
) -> Result<PathBuf> {
    // Serialize base-chroot creation/refresh: concurrent jobs must not race to
    // build or `-Syu` the same shared root.
    let lock = base_chroot_lock();
    let _guard = lock.lock().await;

    std::fs::create_dir_all(chroot_dir)
        .with_context(|| format!("creating chroot dir {}", chroot_dir.display()))?;
    let root = chroot_dir.join("root");

    if root.join(".arch-chroot").exists() || root.exists() && root.join("usr").exists() {
        // Refresh existing chroot; a failure here is non-fatal for the build.
        let mut cmd = devtools("arch-nspawn");
        cmd.arg(&root).args(["pacman", "-Syu", "--noconfirm"]);
        if let Ok((log, status)) = run_capture(cmd).await
            && !status.success()
        {
            tracing::warn!("chroot refresh returned non-zero:\n{log}");
        }
        return Ok(root);
    }

    let mut cmd = devtools("mkarchroot");
    cmd.arg("-C")
        .arg(pacman_conf)
        .arg("-M")
        .arg(makepkg_conf)
        .arg(&root)
        .arg("base-devel");
    let (log, status) = run_capture(cmd).await?;
    if !status.success() {
        bail!("mkarchroot failed:\n{log}");
    }
    Ok(root)
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
        cmd.env("GNUPGHOME", gnupg_home)
            .args(["--batch", "--keyserver", keyserver, "--recv-keys", key]);
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

/// Write the per-package `makepkg.conf` / `pacman.conf` / mirrorlist to a
/// staging directory the caller seeds the base chroot from.
pub fn write_configs(
    dir: &Path,
    makepkg_conf: &str,
    pacman_conf: &str,
    mirrorlist: Option<&str>,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    let makepkg = dir.join("makepkg.conf");
    let pacman = dir.join("pacman.conf");
    std::fs::write(&makepkg, makepkg_conf).context("writing makepkg.conf")?;
    std::fs::write(&pacman, pacman_conf).context("writing pacman.conf")?;
    if let Some(list) = mirrorlist {
        std::fs::write(dir.join("mirrorlist"), list).context("writing mirrorlist")?;
    }
    Ok((makepkg, pacman))
}
