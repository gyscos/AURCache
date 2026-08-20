//! Per-package build execution inside a `devtools` chroot.
//!
//! The heavy lifting (mkarchroot / makechrootpkg / systemd-run) shells out to
//! Arch's `devtools`; those steps only run on a real worker image. The pure
//! helpers here — source extraction, artifact discovery, exit-code
//! classification, command construction — are unit tested.

use anyhow::{Context, Result};
use aurcache_types::worker::{CompleteReport, JobDescriptor};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

/// Extract a `tar.gz` source archive (top-level `{pkgbase}/…`) into `dest` and
/// return the path to the extracted package directory.
pub fn extract_source(archive: &[u8], dest: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest).context("unpacking source archive")?;

    // The archive has a single top-level pkgbase directory.
    std::fs::read_dir(dest)
        .context("reading extracted source")?
        .flatten()
        .find(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .context("source archive had no package directory")
}

/// Find built package artifacts (`*.pkg.tar.*`, excluding detached `.sig`
/// signatures and hidden files) in a build directory.
pub fn discover_artifacts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if is_artifact(&name) {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

/// True for a built package artifact filename.
///
/// The character allow-list matters: these names come from whatever the
/// PKGBUILD wrote into the build directory and are then used as a URL path
/// segment and a repo-tree filename. Restricting to the set alpm actually
/// produces (`pkgname-pkgver-pkgrel-arch.pkg.tar.*`, where epoch contributes
/// `:` and pkgver may contain `+`, `.`, `_`, `~`) keeps a hostile or merely
/// broken PKGBUILD from smuggling separators or traversal through either.
pub fn is_artifact(name: &str) -> bool {
    !name.starts_with('.')
        && name.contains(".pkg.tar")
        && !name.ends_with(".sig")
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '~' | '@')
        })
}

/// Map a build process exit status into a terminal report.
pub fn classify_exit(status: ExitStatus, canceled: bool) -> CompleteReport {
    if canceled {
        return CompleteReport {
            success: false,
            exit_code: status.code(),
            reason: Some("build canceled".to_string()),
            canceled: true,
        };
    }
    if status.success() {
        return CompleteReport {
            success: true,
            exit_code: Some(0),
            reason: None,
            canceled: false,
        };
    }
    let code = status.code();
    let reason = match code {
        Some(137) => "build killed (OOM, exit 137)".to_string(),
        Some(124) => "build timed out (exit 124)".to_string(),
        Some(c) => format!("build failed (exit {c})"),
        None => "build terminated by signal".to_string(),
    };
    CompleteReport {
        success: false,
        exit_code: code,
        reason: Some(reason),
        canceled: false,
    }
}

/// A failure report for an error that happened before/around the build itself
/// (e.g. source download or chroot setup failed).
pub fn setup_failure(reason: impl std::fmt::Display) -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code: None,
        reason: Some(reason.to_string()),
        canceled: false,
    }
}

/// A terminal report for a build aborted before it started (cancel observed
/// during setup).
pub fn classify_exit_canceled() -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code: None,
        reason: Some("build canceled".to_string()),
        canceled: true,
    }
}

/// A terminal report for a build the worker killed after exceeding its timeout.
pub fn timeout_failure(secs: u64) -> CompleteReport {
    CompleteReport {
        success: false,
        exit_code: Some(124),
        reason: Some(format!("build timed out after {secs}s")),
        canceled: false,
    }
}

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
pub fn build_command(
    chroot_root: &Path,
    copy_label: &str,
    srcdest: Option<&Path>,
    build_flags: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "makechrootpkg".to_string(),
        "-c".to_string(),
        "-r".to_string(),
        chroot_root.display().to_string(),
        "-l".to_string(),
        copy_label.to_string(),
    ];
    if let Some(src) = srcdest {
        argv.push("-d".to_string());
        argv.push(format!("{}:/srcdest", src.display()));
    }
    // Separator, then makepkg args (if any).
    if !build_flags.is_empty() {
        argv.push("--".to_string());
        argv.extend(build_flags.iter().cloned());
    }
    argv
}

/// Names of the packages we expect this job to emit (pkgbase; individual
/// pkgnames are validated server-side).
pub fn describe(job: &JobDescriptor) -> String {
    format!("{} ({})", job.pkgbase, job.arch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_tar_gz(pkgbase: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            for (name, content) in files {
                let path = format!("{pkgbase}/{name}");
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append_data(&mut header, path, content.as_bytes())
                    .unwrap();
            }
            tar.finish().unwrap();
        }
        buf
    }

    #[test]
    fn extracts_source_returns_pkgdir() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_tar_gz("hello", &[("PKGBUILD", "pkgname=hello")]);
        let pkgdir = extract_source(&archive, dir.path()).unwrap();
        assert_eq!(pkgdir.file_name().unwrap(), "hello");
        assert!(pkgdir.join("PKGBUILD").exists());
    }

    #[test]
    fn discovers_only_real_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "hello-1.0-1-x86_64.pkg.tar.zst",
            "hello-1.0-1-x86_64.pkg.tar.zst.sig",
            "PKGBUILD",
            ".hidden.pkg.tar.zst",
        ] {
            let mut f = std::fs::File::create(dir.path().join(name)).unwrap();
            f.write_all(b"x").unwrap();
        }
        let found = discover_artifacts(dir.path());
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .to_string_lossy()
                .ends_with("hello-1.0-1-x86_64.pkg.tar.zst")
        );
    }

    #[test]
    fn is_artifact_rules() {
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.xz"));
        assert!(!is_artifact("a-1-1-x86_64.pkg.tar.zst.sig"));
        assert!(!is_artifact(".x.pkg.tar.zst"));
        assert!(!is_artifact("PKGBUILD"));
    }

    /// Real alpm version syntax must survive the character allow-list.
    #[test]
    fn is_artifact_accepts_epoch_and_pkgver_punctuation() {
        assert!(is_artifact("foo-2:1.0_beta+3~rc1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("lib32-gcc-libs-14.2-1-x86_64.pkg.tar.zst"));
    }

    /// Names that would corrupt the upload URL or escape the repo directory.
    #[test]
    fn is_artifact_rejects_url_and_path_hostile_names() {
        assert!(!is_artifact("evil?x=1.pkg.tar.zst"));
        assert!(!is_artifact("evil#frag.pkg.tar.zst"));
        assert!(!is_artifact("../../etc/passwd.pkg.tar.zst"));
        assert!(!is_artifact("a b-1-1-x86_64.pkg.tar.zst"));
        assert!(!is_artifact("a%2f-1-1-x86_64.pkg.tar.zst"));
    }

    #[test]
    fn classifies_success() {
        let r = classify_exit(fake_status(0), false);
        assert!(r.success);
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn classifies_oom_as_terminal_failure() {
        let r = classify_exit(fake_status(137), false);
        assert!(!r.success);
        assert_eq!(r.exit_code, Some(137));
        assert!(r.reason.unwrap().contains("OOM"));
        assert!(!r.canceled);
    }

    #[test]
    fn classifies_cancel() {
        let r = classify_exit(fake_status(1), true);
        assert!(!r.success);
        assert!(r.canceled);
    }

    #[test]
    fn build_command_includes_srcdest_and_flags() {
        let cmd = build_command(
            Path::new("/chroot"),
            "job-42",
            Some(Path::new("/cache/src")),
            &["--nocheck".to_string()],
        );
        let joined = cmd.join(" ");
        assert_eq!(cmd[0], "makechrootpkg");
        assert!(!cmd.iter().any(|a| a == "systemd-run"));
        assert!(!cmd.iter().any(|a| a == "--pkgdest"));
        assert!(joined.contains("makechrootpkg -c -r /chroot -l job-42"));
        assert!(joined.contains("/cache/src:/srcdest"));
        assert!(joined.contains("-- --nocheck"));
    }

    #[test]
    fn build_command_without_srcdest() {
        let cmd = build_command(Path::new("/chroot"), "job-1", None, &[]);
        assert_eq!(cmd[0], "makechrootpkg");
        assert!(!cmd.iter().any(|a| a == "-d"));
        assert!(!cmd.iter().any(|a| a == "--"));
    }

    #[cfg(unix)]
    fn fake_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }
}
