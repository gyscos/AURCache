use crate::settings::general::SettingsTraits;
use aurcache_common::settings::{ApplicationSettings, Setting};
use sea_orm::DatabaseConnection;
use std::fmt::Write as _;
use std::path::Path;

/// Canonical mirrorlist locations, re-exported from `aurcache-deps`.
///
/// They are defined there rather than here because `aurcache-db`'s
/// dependency-backfill migration builds an `AurClient`, so `aurcache-deps`
/// cannot depend on this crate. Re-exporting keeps one import path for
/// everyone else.
pub use aurcache_deps::paths::{
    mirrorlist_dir, mirrorlist_file_name, mirrorlist_path, native_arch, shared_mirrorlist_path,
};

/// Build the makepkg.conf for a build.
///
/// User-provided content (from the `makepkg_conf` setting) is written first.
/// PKGDEST, MAKEFLAGS, PACKAGER, and OPTIONS are always appended at the end so
/// the user cannot accidentally override them — without the right PKGDEST the
/// build can't be collected from the shared mount, and without a valid
/// PACKAGER the generated `desc` file cannot be parsed by libalpm.
///
/// `OPTIONS=(!debug)` suppresses makepkg's split `<pkgname>-debug` packages.
/// Arch's stock `makepkg.conf` enables `debug`, so a worker on distro defaults
/// emits one per package. AURCache serves a single flat repo per architecture,
/// so those would show up in `pacman -Ss` alongside real packages — Arch itself
/// keeps them out of `core`/`extra` and ships them in separate opt-in `*-debug`
/// repos. Publishing debug symbols would mean a second repo, not extra entries
/// in this one.
///
/// Pass `None` for `db_ctx` when no database is available (e.g. the
/// test-builder binary); user config is then skipped.
pub async fn create_makepkg_config(
    db_ctx: Option<(&DatabaseConnection, i32)>,
    pkgdest_dir_base: &Path,
) -> String {
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

    let _ = write!(
        config,
        "MAKEFLAGS=-j$(nproc)\nPKGDEST={}\nPACKAGER='AURCache <aurcache@localhost>'\nOPTIONS=(!debug)\n",
        pkgdest_dir_base.display()
    );

    config
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
pub fn base_pacman_config() -> String {
    "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\nArchitecture = auto\n\n\
     [core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
     [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
     [multilib]\nInclude = /etc/pacman.d/mirrorlist\n"
        .to_string()
}

/// Build the pacman.conf written inside the build container.
///
/// No `[repo]` section is emitted: the worker appends one rendered from the
/// template it received at registration, because only the worker knows which
/// address it reaches this server on.
pub async fn create_pacman_config(db: &DatabaseConnection, pkg_id: i32) -> String {
    let user_conf = ApplicationSettings::get::<String>(Setting::PacmanConf, Some(pkg_id), db)
        .await
        .value;

    if user_conf.trim().is_empty() {
        base_pacman_config()
    } else {
        format!("[options]\nDisableSandbox\n{user_conf}")
    }
}

/// Resolve the mirrorlist content AURCache holds for a given architecture.
///
/// Reads `mirrorlist_dir/mirrorlist.<arch>`; a missing or empty file yields
/// `None` (the worker then falls back to its image's built-in mirrorlist),
/// never an error.
///
/// No architecture is special-cased: an arch is supported exactly when its file
/// exists. Only `x86_64` is written today, so every other arch returns `None` on
/// its own, and adding one later needs no change here.
pub async fn mirrorlist_for(arch: &str, mirrorlist_dir: &Path) -> Option<String> {
    let path = mirrorlist_dir.join(mirrorlist_file_name(arch));
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
) -> (String, String) {
    let makepkg_conf = create_makepkg_config(Some((db, pkg_id)), pkgdest_dir).await;
    let pacman_conf = create_pacman_config(db, pkg_id).await;
    (makepkg_conf, pacman_conf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server never emits a `[repo]` section any more; the worker appends
    /// one for the host it actually reaches this server on.
    #[test]
    fn base_pacman_config_has_expected_markers_and_no_repo() {
        let conf = base_pacman_config();
        assert!(conf.contains("DisableSandbox"));
        assert!(conf.contains("SigLevel = Never"));
        assert!(conf.contains("Include = /etc/pacman.d/mirrorlist"));
        assert!(!conf.contains("[repo]"));
    }

    #[tokio::test]
    async fn mirrorlist_for_unpopulated_arch_is_none() {
        let dir = std::env::temp_dir();
        assert!(mirrorlist_for("aarch64", &dir).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mirrorlist_for("x86_64", dir.path()).await.is_none());
    }

    /// The layout must be genuinely arch-keyed, not x86_64 with extra steps:
    /// writing another arch's file is all it should take to support it.
    #[tokio::test]
    async fn mirrorlist_for_reads_any_populated_arch() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("mirrorlist.aarch64"),
            "Server = https://arm.example/$repo/os/$arch\n",
        )
        .await
        .unwrap();

        assert!(mirrorlist_for("aarch64", dir.path()).await.is_some());
        // ...and it must not leak across architectures.
        assert!(mirrorlist_for("x86_64", dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn mirrorlist_for_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("mirrorlist.x86_64"),
            "Server = https://mirror/$repo/os/$arch\n",
        )
        .await
        .unwrap();
        let content = mirrorlist_for("x86_64", dir.path()).await;
        assert!(content.unwrap().contains("Server = https://mirror"));
    }
}
