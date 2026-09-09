//! Per-package build execution inside a `devtools` chroot.
//!
//! The heavy lifting (mkarchroot / makechrootpkg) shells out to Arch's
//! `devtools`; those steps only run on a real worker image. Command
//! construction, the one pure part, is unit tested here. Source extraction,
//! artifact discovery and exit classification are executor-independent and
//! live in `aurcache_worker_core`.

use std::path::{Path, PathBuf};

/// Build the `makechrootpkg` argv for a per-package build.
///
/// The worker runs inside a container, so resource isolation (memory) is the
/// container's responsibility and a build timeout is enforced worker-side by
/// killing the child — no `systemd-run`/cgroup wrapper is used.
///
/// `makechrootpkg` copies the built packages into its **current working
/// directory**, so the caller sets `cwd` to the desired destination (there is
/// no `--pkgdest` flag on `makepkg`). Any `build_flags` are forwarded to
/// `makepkg` after the `--` separator.
///
/// Builds run as `build_user`, which must not be the user the worker itself
/// runs as: file ownership is what keeps the worker's identity and credentials
/// out of reach of the code a PKGBUILD executes.
///
/// `binds` become `-d src:dest` arguments. devtools appends these *after* its
/// own binds, so a bind here overrides one devtools made for the same target —
/// which is how the per-job pacman cache replaces the shared one. There is no
/// supported flag for that; `SRCDEST`, which does have one, uses it instead.
/// Where to find the sandbox-wrapping `makechrootpkg`.
///
/// An absolute path, not a name on `PATH`. The worker runs devtools through
/// `sudo`, and sudo replaces `PATH` with its own `secure_path`, so a `PATH`
/// set for the service never reaches the command. Naming the file is the only
/// way to be sure which one runs.
///
/// This is *not* the packaged `/usr/bin/makechrootpkg`: it is a copy with the
/// two places a PKGBUILD is executed outside the chroot wrapped in
/// `aurcache-sandbox` (see `packaging/patch-makechrootpkg.py`). Running the
/// unpatched one would build packages with those two steps unconfined, which is
/// a silent loss of isolation rather than a failure -- so the default points at
/// the patched copy and an operator who moves it says where it went.
fn makechrootpkg_path() -> String {
    std::env::var("WORKER_MAKECHROOTPKG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/usr/lib/aurcache/bin/makechrootpkg".to_string())
}

pub fn build_command(
    chroot_root: &Path,
    copy_label: &str,
    devtools_owns_copy: bool,
    binds: &[(PathBuf, PathBuf)],
    build_flags: &[String],
    build_user: &str,
) -> Vec<String> {
    let mut argv = vec![makechrootpkg_path()];
    if devtools_owns_copy {
        argv.push("-c".to_string());
    }
    argv.extend([
        "-r".to_string(),
        chroot_root.display().to_string(),
        "-l".to_string(),
        copy_label.to_string(),
    ]);
    // Delete the copy when the build ends, which devtools only does for a
    // temporary chroot: `delete_chroot` is called under `(( temp_chroot ))`
    // and nowhere else. Without it every build left a full chroot behind for
    // good -- 26 of them, 362G, until the disk filled and took a four-hour
    // build with it.
    //
    // Ordering matters: `-T` appends `-$$` to the copy name and `-l` *assigns*
    // it, so a `-T` before the label would have its suffix overwritten and the
    // copy would outlive the build after all.
    //
    // Letting devtools do the deleting rather than doing it ourselves is what
    // keeps this correct on btrfs, where the copy is a subvolume snapshot that
    // `rm -rf` cannot remove. It is conditional on devtools having made the
    // copy in the first place: a strategy that makes its own must also take it
    // down, and asking it to delete what it did not create fails the build.
    if devtools_owns_copy {
        argv.push("-T".to_string());
    }
    // Name the build user explicitly. Without `-U`, makechrootpkg infers it
    // from `SUDO_USER`, which is whoever invoked us -- so the worker's own user
    // would run the build, and a build could then read the worker's mTLS
    // identity and credentials by ordinary file permissions.
    argv.extend(["-U".to_string(), build_user.to_string()]);
    // `SRCDEST` is passed through the environment instead of a bind mount:
    // devtools reads it directly and binds it itself, which avoids competing
    // with its own `--bind=$SRCDEST:/srcdest`.
    for (host, chroot) in binds {
        argv.push("-d".to_string());
        argv.push(format!("{}:{}", host.display(), chroot.display()));
    }
    // Separator, then makepkg args (if any).
    if !build_flags.is_empty() {
        argv.push("--".to_string());
        argv.extend(build_flags.iter().cloned());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_command_includes_flags() {
        let cmd = build_command(
            Path::new("/chroot"),
            "job-42",
            true,
            &[],
            &["--nocheck".to_string()],
            "builder",
        );
        let joined = cmd.join(" ");
        // An absolute path, not a bare name: the command goes through `sudo`,
        // which replaces PATH with its own `secure_path`, so a name would
        // resolve to the *unpatched* system makechrootpkg and run two steps of
        // every build unconfined.
        assert!(
            cmd[0].starts_with('/') && cmd[0].ends_with("/makechrootpkg"),
            "makechrootpkg must be named by absolute path, got {}",
            cmd[0]
        );
        assert!(!cmd.iter().any(|a| a == "systemd-run"));
        assert!(!cmd.iter().any(|a| a == "--pkgdest"));
        assert!(joined.contains("makechrootpkg -c -r /chroot -l job-42"));
        // Builds must never inherit the worker's user via SUDO_USER.
        assert!(joined.contains("-U builder"));
        // The copy has to be temporary, or it is never deleted.
        assert!(joined.contains(" -T "), "{joined}");
        // ... and `-T` must follow `-l`, which assigns the name it suffixes.
        assert!(
            joined.find(" -l ") < joined.find(" -T "),
            "-T must come after -l: {joined}"
        );
        assert!(joined.contains("-- --nocheck"));
    }

    /// A strategy that makes the copy itself leaves devtools' half out: no
    /// `-c` to copy the base over it, and no `-T` to delete what devtools did
    /// not create. Everything else about the command is the same, which is the
    /// point of asking the lease rather than branching at the call site.
    #[test]
    fn a_caller_owned_copy_drops_the_devtools_flags() {
        let cmd = build_command(Path::new("/chroot"), "job-9", false, &[], &[], "builder");
        let joined = cmd.join(" ");
        assert!(!cmd.iter().any(|a| a == "-c"), "{joined}");
        assert!(!cmd.iter().any(|a| a == "-T"), "{joined}");
        assert!(joined.contains("-r /chroot -l job-9"), "{joined}");
        assert!(joined.contains("-U builder"), "{joined}");
    }

    /// `SRCDEST` travels in the environment, not as a bind: devtools binds it
    /// itself, so adding one here would compete with devtools' own bind.
    #[test]
    fn build_command_does_not_bind_srcdest() {
        let cmd = build_command(Path::new("/chroot"), "job-1", true, &[], &[], "builder");
        assert!(!cmd.join(" ").contains("/srcdest"));
        assert!(!cmd.iter().any(|a| a == "-d"));
        assert!(!cmd.iter().any(|a| a == "--"));
    }

    /// Credentials reach the build as an extra bind mount, alongside SRCDEST.
    #[test]
    fn build_command_adds_bind_mounts() {
        let binds = vec![
            (
                PathBuf::from("/job/secrets"),
                PathBuf::from("/build-secrets"),
            ),
            (PathBuf::from("/host/netrc"), PathBuf::from("/etc/netrc")),
        ];
        let cmd = build_command(Path::new("/chroot"), "job-7", true, &binds, &[], "builder");
        let joined = cmd.join(" ");
        assert!(joined.contains("/job/secrets:/build-secrets"));
        assert!(joined.contains("/host/netrc:/etc/netrc"));
        // Bind mounts are not makepkg flags; no separator should appear.
        assert!(!cmd.iter().any(|a| a == "--"));
    }
}
