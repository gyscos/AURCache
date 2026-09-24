//! Keep package builds off the worker-protocol port.
//!
//! Builds share the worker host's network namespace (systemd-nspawn runs
//! without `--network-*` flags), so a crafted PKGBUILD can reach the server's
//! worker port: it learns the address from its own `pacman.conf`, fetches the
//! public CA, and submits a rogue registration under a spoofed name. That
//! lands `pending`, one mistaken approval from a valid mTLS certificate.
//!
//! The repo, the AUR and the internet must stay reachable, so the block is by
//! (`uid`, `port`) rather than by host: every build runs as the unprivileged
//! build user (`WORKER_BUILD_USER`, `builder` by default), while the worker
//! itself runs as `aurcache`. One `OUTPUT` rule per address family rejects the
//! build user's TCP traffic to the worker port the server is reached on, and
//! nothing else. Set `WORKER_BUILD_FIREWALL=0` to skip this (exotic network
//! setups); the worker logs a warning either way when the rule cannot be
//! installed, and never refuses to start over it.

use anyhow::{Context, Result};
use std::process::Command;

/// Environment switch that disables the rule; see the module docs.
const DISABLE_ENV: &str = "WORKER_BUILD_FIREWALL";

/// Port assumed when `AURCACHE_URL` names none.
const DEFAULT_WORKER_PORT: u16 = aurcache_common::ports::AURCACHE_WORKER_PORT;

/// Whether the operator disabled the rule.
#[must_use]
pub fn disabled() -> bool {
    matches!(
        std::env::var(DISABLE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Worker-protocol port parsed from the server URL the worker dials.
///
/// Takes the explicit port when present, else the compiled default. Only the
/// host part is inspected, so paths, queries and userinfo cannot smuggle a
/// different value in.
#[must_use]
pub fn worker_port_from_url(url: &str) -> u16 {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = after_scheme
        .split('/')
        .next()
        .unwrap_or(after_scheme)
        .trim_end_matches(':');
    host_port
        .rsplit(':')
        .next()
        .and_then(|port| {
            // An IPv6 literal without a port (`[::1]`) leaves the bracketed
            // address as the last segment; only a purely numeric tail counts.
            if port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() {
                port.parse::<u16>().ok()
            } else {
                None
            }
        })
        .unwrap_or(DEFAULT_WORKER_PORT)
}

/// This process's own uid: the rule must never name it (see below).
fn own_uid() -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("running id -u")?;
    anyhow::ensure!(
        output.status.success(),
        "id -u failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .context("id -u printed no numeric uid")
}

/// Refuse a rule that would match this process itself.
///
/// A `WORKER_BUILD_USER` naming the worker's own account (or root, via
/// `sudo -u` inheritance accidents) would firewall the worker off its own
/// server: enrollment, claims and heartbeats would all fail closed with
/// nothing pointing at this rule. The build user exists precisely to be
/// someone else; overlapping it is a misconfiguration, and loud is better
/// than a mysteriously catatonic worker.
fn check_not_self(build_uid: u32, own_uid: u32, build_user: &str) -> Result<()> {
    anyhow::ensure!(
        build_uid != own_uid,
        "build user {build_user:?} resolves to this process's own uid ({own_uid}); \
         refusing a rule that would block the worker itself"
    );
    Ok(())
}

/// Numeric uid of a local user, resolved at runtime.
///
/// The build user's uid is assigned dynamically (systemd-sysusers `-`), so it
/// must never be hardcoded: this host gave `builder` 967, another may not.
fn uid_of_user(username: &str) -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .arg(username)
        .output()
        .with_context(|| format!("running id -u {username}"))?;
    anyhow::ensure!(
        output.status.success(),
        "id -u {username} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .with_context(|| format!("id -u {username} printed no numeric uid"))
}

/// The `OUTPUT` rule that confines the build user, as argv after the tool.
///
/// Idempotent installation is check-then-add: `-C` reports presence, `-I`
/// inserts at the head only when absent.
fn rule_args(uid: u32, port: u16) -> Vec<String> {
    vec![
        "OUTPUT".to_string(),
        "-m".to_string(),
        "owner".to_string(),
        "--uid-owner".to_string(),
        uid.to_string(),
        "-p".to_string(),
        "tcp".to_string(),
        "--dport".to_string(),
        port.to_string(),
        "-j".to_string(),
        "REJECT".to_string(),
    ]
}

fn run_sudo(
    tool: &str,
    args: &[String],
    path_override: Option<&str>,
) -> Result<std::process::Output> {
    let mut command = Command::new("sudo");
    // Tests point this at a stub toolbox; production always passes `None`,
    // which inherits this process's `PATH` untouched.
    if let Some(path) = path_override {
        command.env("PATH", path);
    }
    command
        .arg(tool)
        .args(args)
        .output()
        .with_context(|| format!("running sudo {tool}"))
}

/// Ensure the rule exists for one address family (`iptables` or `ip6tables`).
fn ensure_one(tool: &str, uid: u32, port: u16) -> Result<()> {
    ensure_one_in(tool, uid, port, None)
}

fn ensure_one_in(tool: &str, uid: u32, port: u16, path_override: Option<&str>) -> Result<()> {
    let args = rule_args(uid, port);
    let mut check = vec!["-C".to_string()];
    check.extend(args.clone());
    if run_sudo(tool, &check, path_override)?.status.success() {
        return Ok(());
    }
    let mut add = vec!["-I".to_string()];
    add.extend(args);
    let output = run_sudo(tool, &add, path_override)?;
    anyhow::ensure!(
        output.status.success(),
        "{} refused the build-user worker-port rule: {}",
        tool,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Block the build user's TCP traffic to the worker-protocol port.
///
/// Resolves the build user's uid and the port from `server_url`, then ensures
/// the rule via `iptables` and `ip6tables` (each best-effort: a v6-less host
/// fails only its own leg). A no-op when disabled; an `Err` names what failed
/// so the caller can warn and continue.
///
/// The rule is intentionally never removed — not on shutdown, not on package
/// removal. It names only the build user's uid and one destination port, so a
/// stale rule affects nobody else, while remove-on-stop could never be
/// reliable anyway: a killed worker would leave it behind, and two workers
/// sharing a build user would race to delete each other's. Restarts are safe
/// because installation is check-then-add.
pub fn ensure(server_url: &str, build_user: &str) -> Result<()> {
    if disabled() {
        tracing::info!("{DISABLE_ENV} disables the build-user worker-port rule; skipping");
        return Ok(());
    }
    let uid = uid_of_user(build_user)?;
    check_not_self(uid, own_uid()?, build_user)?;
    let port = worker_port_from_url(server_url);
    ensure_one("iptables", uid, port)?;
    match ensure_one("ip6tables", uid, port) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::debug!("ip6tables leg of the worker-port rule not installed: {e:#}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn port_comes_from_the_url_or_falls_back() {
        assert_eq!(worker_port_from_url("https://aurcache:8083"), 8083);
        assert_eq!(
            worker_port_from_url("https://build.example.com:9090/api"),
            9090
        );
        assert_eq!(worker_port_from_url("https://aurcache:8083/"), 8083);
        assert_eq!(worker_port_from_url("https://example.com"), 8083);
        assert_eq!(worker_port_from_url("https://example.com/api"), 8083);
        assert_eq!(worker_port_from_url("https://[::1]:8083"), 8083);
        assert_eq!(worker_port_from_url("https://[::1]/api"), 8083);
        assert_eq!(worker_port_from_url("not a url"), 8083);
    }

    #[test]
    fn a_rule_naming_our_own_uid_is_refused() {
        assert!(check_not_self(967, 1000, "builder").is_ok());
        let err = check_not_self(1000, 1000, "aurcache").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("own uid"), "{message}");
        assert!(message.contains("aurcache"), "{message}");
    }

    #[test]
    fn rule_names_uid_and_port_and_nothing_else() {
        assert_eq!(
            rule_args(967, 8083),
            [
                "OUTPUT",
                "-m",
                "owner",
                "--uid-owner",
                "967",
                "-p",
                "tcp",
                "--dport",
                "8083",
                "-j",
                "REJECT"
            ]
            .map(str::to_string)
            .to_vec()
        );
    }

    /// A fake `sudo` + `iptables` pair records invocations and answers `-C`
    /// from a state file, so `ensure_one_in` runs hermetically with a
    /// child-scoped `PATH` (this process's environment is never touched).
    /// Note `$2`, not `$1`: the stub runs as `sudo iptables …`, so the tool
    /// name itself occupies `$1`.
    fn fake_toolbox() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let state = dir.path().join("present");
        std::fs::write(
            dir.path().join("sudo"),
            format!(
                "#!/bin/sh\nexec \"{self_}/iptables\" \"$@\"\n",
                self_ = dir.path().display()
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("iptables"),
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{log}\"\n\
                 if [ \"$2\" = \"-C\" ]; then [ -f \"{state}\" ]; exit $?; fi\n\
                 touch \"{state}\"; exit 0\n",
                log = log.display(),
                state = state.display()
            ),
        )
        .unwrap();
        for tool in ["sudo", "iptables"] {
            let path = dir.path().join(tool);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = dir.path().to_path_buf();
        (dir, bin)
    }

    #[test]
    fn ensure_is_check_then_add_then_stable() {
        let (_dir, bin) = fake_toolbox();
        // Child-scoped `PATH`: the fake `sudo`/`iptables` are found by the
        // stubbed runs only, and the real environment is never mutated.
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let path = Some(path.as_str());
        ensure_one_in("iptables", 967, 8083, path).unwrap();
        let first = std::fs::read_to_string(bin.join("calls.log")).unwrap();
        assert!(first.contains("-C OUTPUT"), "checks first:\n{first}");
        assert!(first.contains("-I OUTPUT"), "adds when absent:\n{first}");
        ensure_one_in("iptables", 967, 8083, path).unwrap();
        let second = std::fs::read_to_string(bin.join("calls.log")).unwrap();
        assert_eq!(
            second.matches("-I OUTPUT").count(),
            1,
            "no duplicate insert:\n{second}"
        );
        assert!(
            second.contains("--uid-owner 967"),
            "rule names the build uid:\n{second}"
        );
        assert!(
            second.contains("--dport 8083"),
            "rule names the worker port:\n{second}"
        );
    }
}
