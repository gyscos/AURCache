//! `build-once`: build a local PKGBUILD directory in a chroot without any
//! server, for fast developer iteration. Reuses the same chroot machinery as
//! the polling path.

use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::build;
use crate::cache::Cache;
use crate::chroot;
use crate::config::Config;

/// Build the PKGBUILD in `path` locally and report where the artifacts landed.
pub async fn build_once(cfg: &Config, path: &Path, flags: &[String]) -> Result<()> {
    let pkgdir = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if !pkgdir.join("PKGBUILD").exists() {
        bail!("no PKGBUILD found in {}", pkgdir.display());
    }
    tracing::info!("Building {} locally", pkgdir.display());

    // Seed the base chroot from the host's default configs.
    let pacman_conf = existing("/etc/pacman.conf")?;
    let makepkg_conf = existing("/etc/makepkg.conf")?;
    chroot::ensure_base_chroot(&cfg.chroot_dir, &pacman_conf, &makepkg_conf)
        .await
        .context("preparing base chroot")?;

    let cache = Cache::new(&cfg.cache_dir, cfg.cache_max_size, cfg.cache_ttl);
    let pkgbase = pkgdir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("local");
    let srcdest = cache.srcdest(pkgbase);

    let argv = build::build_command(
        &cfg.chroot_dir,
        "build-once",
        srcdest.as_deref(),
        flags,
    );
    tracing::info!("$ sudo {}", argv.join(" "));

    let status = chroot::devtools(&argv[0])
        .args(&argv[1..])
        .current_dir(&pkgdir)
        .status()
        .await
        .context("running build")?;

    let report = build::classify_exit(status, false);
    if report.success {
        let artifacts = build::discover_artifacts(&pkgdir);
        tracing::info!("Build succeeded: {} artifact(s)", artifacts.len());
        for a in artifacts {
            println!("{}", a.display());
        }
        Ok(())
    } else {
        bail!(
            "build failed: {}",
            report.reason.as_deref().unwrap_or("unknown")
        );
    }
}

fn existing(path: &str) -> Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(path);
    if p.exists() {
        Ok(p)
    } else {
        bail!("{path} not found (run on an Arch host/image)");
    }
}
