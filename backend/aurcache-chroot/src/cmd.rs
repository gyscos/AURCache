//! Running the few commands that need root.
//!
//! The worker runs as an unprivileged user with `sudo` (see
//! `packaging/aurcache-worker.sudoers` for what that grant is and is not), and
//! every privileged action here goes through [`privileged`], so each one is a
//! single, logged argument vector. Run as root -- the hybrid image's
//! entrypoint, a test under `sudo` -- the command is run directly.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use tokio::process::Command;

/// Whether this process is already root, in which case `sudo` is skipped.
fn is_root() -> bool {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The command line for `args`, as root.
fn command<S: AsRef<OsStr>>(args: &[S]) -> Command {
    let (program, rest) = args.split_first().expect("a command to run");
    if is_root() {
        let mut cmd = Command::new(program);
        cmd.args(rest);
        cmd
    } else {
        let mut cmd = Command::new("sudo");
        // `-n`: a missing grant is an error to report, never a password prompt
        // that hangs a worker with no terminal.
        cmd.arg("-n").args(args);
        cmd
    }
}

/// Run `args` as root and return its stdout, or an error carrying its stderr.
pub async fn privileged<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    let shown = display(args);
    tracing::debug!("$ {shown}");
    let output = command(args)
        .output()
        .await
        .with_context(|| format!("running {shown}"))?;
    if !output.status.success() {
        bail!(
            "{shown} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run an unprivileged query and return its stdout.
pub async fn query<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    let shown = display(args);
    let (program, rest) = args.split_first().expect("a command to run");
    let output = Command::new(program)
        .args(rest)
        .output()
        .await
        .with_context(|| format!("running {shown}"))?;
    if !output.status.success() {
        bail!(
            "{shown} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn display<S: AsRef<OsStr>>(args: &[S]) -> String {
    args.iter()
        .map(|a| a.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
