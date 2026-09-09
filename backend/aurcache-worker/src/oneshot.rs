//! `build-once`: build a local PKGBUILD directory in a chroot without any
//! server, for fast developer iteration. Reuses the same chroot machinery as
//! the polling path.

use anyhow::{Context, Result, bail};
use std::path::Path;

use aurcache_worker_core::{artifacts, report};

use crate::build;
use crate::cache::Cache;
use crate::chroot;
use crate::chroots::Chroots;
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

    // Seed the base chroot from the host's pacman.conf: a local one-shot build
    // is explicitly "build this the way this machine would", so the host's
    // repositories are the right ones. `makepkg.conf` is still left to the
    // chroot's own, matching what a served build gets.
    let pacman_conf = existing("/etc/pacman.conf")?;
    let chroots = Chroots::new(cfg.chroot_dir.clone());
    chroots
        .refresh(&pacman_conf)
        .await
        .context("preparing base chroot")?;
    let lease = chroots.acquire("build-once").await?;

    let cache = Cache::new(
        &cfg.cache_dir,
        cfg.cache_max_size,
        cfg.cache_ttl,
        cfg.pkgcache_max_size,
        cfg.pkgcache_ttl,
    );
    let pkgbase = pkgdir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("local");
    let srcdest = cache.srcdest(pkgbase);

    let argv = build::build_command(
        lease.chroot_dir(),
        lease.label(),
        lease.devtools_owns_copy(),
        &cfg.bind_mounts,
        flags,
        &cfg.build_user,
    );
    tracing::info!("$ sudo {}", argv.join(" "));

    let mut cmd = chroot::devtools(&argv[0]);
    cmd.args(&argv[1..]).current_dir(&pkgdir);
    // devtools binds `$SRCDEST` itself; unset, it falls back to the PKGBUILD
    // directory and downloads are not cached between runs.
    if let Some(dir) = srcdest.as_deref() {
        cmd.env("SRCDEST", dir);
    }
    let status = cmd.status().await.context("running build")?;
    // After the child has exited, never before.
    lease.release().await;

    let report = report::classify_exit(status, false);
    if report.success {
        let artifacts = artifacts::discover_artifacts(&pkgdir);
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
