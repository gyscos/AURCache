//! Base `devtools` chroot lifecycle and per-job execution helpers that shell
//! out to Arch's `devtools`. These run only on a real privileged worker image;
//! the pure argument/parse helpers they rely on live in [`crate::build`].

use anyhow::{Context, Result, bail};
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
        return Ok(root);
    }

    let mut cmd = devtools("mkarchroot");
    cmd.arg("-C")
        .arg(pacman_conf)
        .arg("-M")
        .arg(makepkg_conf)
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

/// This worker image's own makepkg defaults, used as the base layer for the
/// job's `makepkg.conf`.
const SYSTEM_MAKEPKG_CONF: &str = "/etc/makepkg.conf";

/// Layer the server's makepkg settings on top of a full default `makepkg.conf`.
///
/// The server sends only what it wants to force (`PKGDEST`, `MAKEFLAGS`,
/// `PACKAGER`, plus any user-provided `makepkg.conf` setting). That was fine
/// when it was written to `~/.config/pacman/makepkg.conf`, which makepkg sources
/// *after* the system file — but it is now handed to `mkarchroot -M`, which
/// installs it as the chroot's **entire** `/etc/makepkg.conf`. On its own it
/// leaves makepkg with no `PKGEXT`/`SRCEXT`/`CARCH`/compression settings, and
/// every build dies with:
///
/// ```text
/// ==> ERROR: $PKGEXT does not contain a valid package suffix (needs '.pkg.tar*', got '')
/// ==> ERROR: Could not download sources.
/// ```
///
/// The defaults have to come from the worker rather than the server: they are
/// architecture-specific (`CARCH`, `CHOST`, `CFLAGS`), and the server may
/// dispatch a job to a worker of a different architecture than its own.
/// Overrides are appended last so they still win.
fn merge_makepkg_conf(system_defaults: Option<&str>, overrides: &str) -> String {
    let Some(base) = system_defaults else {
        return overrides.to_string();
    };
    let mut out = String::with_capacity(base.len() + overrides.len() + 96);
    out.push_str(base);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# --- AURCache job overrides (applied last, win over defaults) ---\n");
    out.push_str(overrides);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
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

    let system_defaults = std::fs::read_to_string(SYSTEM_MAKEPKG_CONF).ok();
    if system_defaults.is_none() {
        tracing::warn!(
            "{SYSTEM_MAKEPKG_CONF} not readable; using server-provided makepkg.conf alone \
             (builds will fail if it lacks PKGEXT/SRCEXT)"
        );
    }
    let merged = merge_makepkg_conf(system_defaults.as_deref(), makepkg_conf);

    std::fs::write(&makepkg, merged).context("writing makepkg.conf")?;
    std::fs::write(&pacman, pacman_conf).context("writing pacman.conf")?;
    if let Some(list) = mirrorlist {
        std::fs::write(dir.join("mirrorlist"), list).context("writing mirrorlist")?;
    }
    Ok((makepkg, pacman))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed stand-in for the distro file; PKGEXT/SRCEXT are the fields
    /// whose absence makepkg rejects outright.
    const DEFAULTS: &str = "CARCH=\"x86_64\"\nCHOST=\"x86_64-pc-linux-gnu\"\n\
                            PKGEXT='.pkg.tar.zst'\nSRCEXT='.src.tar.gz'\n";

    #[test]
    fn merge_keeps_defaults_makepkg_requires() {
        let merged = merge_makepkg_conf(Some(DEFAULTS), "PKGDEST=/output\n");
        assert!(merged.contains("PKGEXT='.pkg.tar.zst'"));
        assert!(merged.contains("SRCEXT='.src.tar.gz'"));
        assert!(merged.contains("CARCH=\"x86_64\""));
        assert!(merged.contains("PKGDEST=/output"));
    }

    /// Overrides must come after the defaults so they actually take effect —
    /// makepkg.conf is sourced as bash, so the last assignment wins.
    #[test]
    fn overrides_are_appended_after_defaults() {
        let merged = merge_makepkg_conf(
            Some("PKGDEST=/system\nPKGEXT='.pkg.tar.zst'\n"),
            "PKGDEST=/output\n",
        );
        let first = merged.find("PKGDEST=/system").expect("default present");
        let last = merged.find("PKGDEST=/output").expect("override present");
        assert!(last > first, "override must come after the default");
    }

    #[test]
    fn merge_falls_back_to_overrides_when_no_system_file() {
        let merged = merge_makepkg_conf(None, "PKGDEST=/output\n");
        assert_eq!(merged, "PKGDEST=/output\n");
    }

    #[test]
    fn merge_inserts_newline_between_sections() {
        let merged = merge_makepkg_conf(Some("PKGEXT='.pkg.tar.zst'"), "PKGDEST=/output");
        assert!(merged.contains("PKGEXT='.pkg.tar.zst'\n"));
        assert!(merged.ends_with('\n'));
    }
}
