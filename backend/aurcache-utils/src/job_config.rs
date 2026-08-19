use crate::settings::general::SettingsTraits;
use aurcache_types::settings::{ApplicationSettings, Setting};
use sea_orm::DatabaseConnection;
use std::path::{Path, PathBuf};

/// Canonical directory AURCache stores/serves the pacman `mirrorlist` from.
///
/// Single source of truth shared by the mirrorlist writers (startup mirrorlist
/// bootstrap, the mirror-ranking scheduler) and the worker job-config endpoint
/// that serves it. Overridable via `AURCACHE_MIRRORLIST_DIR` (default `./repo`).
#[must_use]
pub fn mirrorlist_dir() -> PathBuf {
    PathBuf::from(std::env::var("AURCACHE_MIRRORLIST_DIR").unwrap_or_else(|_| "./repo".to_string()))
}

/// Build the makepkg.conf for a build.
///
/// User-provided content (from the `makepkg_conf` setting) is written first.
/// PKGDEST, MAKEFLAGS, and PACKAGER are always appended at the end so the
/// user cannot accidentally override them — without the right PKGDEST the
/// build can't be collected from the shared mount, and without a valid
/// PACKAGER the generated `desc` file cannot be parsed by libalpm.
///
/// Pass `None` for `db_ctx` when no database is available (e.g. the
/// test-builder binary); user config is then skipped.
pub async fn create_makepkg_config(
    db_ctx: Option<(&DatabaseConnection, i32)>,
    pkgdest_dir_base: &Path,
) -> anyhow::Result<(String, String)> {
    let mut config = String::new();

    if let Some((db, pkg_id)) = db_ctx {
        let user_conf = ApplicationSettings::get::<String>(Setting::MakepkgConf, Some(pkg_id), db)
            .await
            .value;
        if !user_conf.trim().is_empty() {
            config.push_str(&user_conf);
            if !config.ends_with('\n') {
                config.push('\n');
            }
        }
    }

    config.push_str(&format!(
        "MAKEFLAGS=-j$(nproc)\nPKGDEST={}\nPACKAGER='AURCache <aurcache@localhost>'\n",
        pkgdest_dir_base.display()
    ));

    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((config, makepkg_config_path.to_string()))
}

/// Generate the standard pacman.conf written inside a build container.
///
/// When `aurcache_repo_url` is `Some`, a `[repo]` section pointing at the
/// AURCache package server is appended so makepkg can resolve previously built
/// packages.  Pass `None` for standalone builds (e.g. the test-builder) where
/// no AURCache server is running.
///
/// `DisableSandbox`: pacman 7's Landlock download sandbox cannot initialise
/// inside the unprivileged/nested build chroot and aborts every `pacman -Sy`.
/// Disabling it is required for pacman to run there; it only relaxes pacman's
/// own download isolation, not the surrounding `makechrootpkg` chroot.
pub fn base_pacman_config(aurcache_repo_url: Option<&str>) -> String {
    let base = "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\nArchitecture = auto\n\n\
                [core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                [multilib]\nInclude = /etc/pacman.d/mirrorlist\n";
    match aurcache_repo_url {
        Some(url) => format!("{base}\n[repo]\nSigLevel = Never\nServer = {url}/$arch\n"),
        None => base.to_string(),
    }
}

/// Build the pacman.conf written inside the build container.
///
/// User-provided content replaces the stock repo sections but still gets the
/// AURCache repo appended so makepkg can resolve previously built packages.
pub async fn create_pacman_config(
    db: &DatabaseConnection,
    pkg_id: i32,
    aurcache_repo_url: &str,
) -> String {
    let user_conf = ApplicationSettings::get::<String>(Setting::PacmanConf, Some(pkg_id), db)
        .await
        .value;

    if user_conf.trim().is_empty() {
        base_pacman_config(Some(aurcache_repo_url))
    } else {
        let repo_conf = format!("\n[repo]\nSigLevel = Never\nServer = {aurcache_repo_url}/$arch\n");
        format!("[options]\nDisableSandbox\n{user_conf}{repo_conf}")
    }
}

/// Resolve the mirrorlist content AURCache holds for a given architecture.
///
/// Only `x86_64` is populated today (by the mirror-ranking scheduler job). Other
/// architectures return `None`, in which case a worker falls back to its image's
/// built-in mirrorlist. This is forward-ready: once AURCache stores a per-arch
/// mirrorlist, this function simply returns `Some` for that arch with no change
/// to callers or the worker protocol.
///
/// Reads from `mirrorlist_dir/mirrorlist`; a missing file yields `None`
/// (graceful fallback), never an error.
pub async fn mirrorlist_for(arch: &str, mirrorlist_dir: &Path) -> Option<String> {
    if arch != "x86_64" {
        return None;
    }
    let path = mirrorlist_dir.join("mirrorlist");
    match tokio::fs::read_to_string(&path).await {
        Ok(content) if !content.trim().is_empty() => Some(content),
        _ => None,
    }
}

/// Assemble the self-contained build configuration for a [`JobDescriptor`].
///
/// Returns the rendered `(makepkg.conf, pacman.conf)` a worker injects into its
/// chroot. `aurcache_repo_url` is the public base URL of the AURCache package
/// server; a `[repo]` section pointing at it is appended so the build can
/// resolve previously built packages.
pub async fn build_job_config(
    db: &DatabaseConnection,
    pkg_id: i32,
    pkgdest_dir: &Path,
    aurcache_repo_url: &str,
) -> anyhow::Result<(String, String)> {
    let (makepkg_conf, _path) = create_makepkg_config(Some((db, pkg_id)), pkgdest_dir).await?;
    let pacman_conf = create_pacman_config(db, pkg_id, aurcache_repo_url).await;
    Ok((makepkg_conf, pacman_conf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_pacman_config_has_expected_markers() {
        let conf = base_pacman_config(Some("https://aur.example.com"));
        assert!(conf.contains("DisableSandbox"));
        assert!(conf.contains("SigLevel = Never"));
        assert!(conf.contains("Include = /etc/pacman.d/mirrorlist"));
        assert!(conf.contains("[repo]"));
        assert!(conf.contains("Server = https://aur.example.com/$arch"));
    }

    #[test]
    fn base_pacman_config_without_repo_omits_repo_section() {
        let conf = base_pacman_config(None);
        assert!(conf.contains("DisableSandbox"));
        assert!(!conf.contains("[repo]"));
    }

    #[tokio::test]
    async fn mirrorlist_for_non_x86_is_none() {
        let dir = std::env::temp_dir();
        assert!(mirrorlist_for("aarch64", &dir).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mirrorlist_for("x86_64", dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("mirrorlist"),
            "Server = https://mirror/$repo/os/$arch\n",
        )
        .await
        .unwrap();
        let content = mirrorlist_for("x86_64", dir.path()).await;
        assert!(content.unwrap().contains("Server = https://mirror"));
    }
}
