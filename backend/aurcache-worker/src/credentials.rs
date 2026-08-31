//! Build credentials made available inside the build chroot.
//!
//! Some AUR packages fetch their sources over authenticated transports —
//! `unreal-engine` clones `git+ssh://git@github.com/EpicGames/UnrealEngine`,
//! which requires an SSH key belonging to a GitHub account in the Epic Games
//! organisation. That fetch happens inside the chroot during `makepkg`, not on
//! the server, so the key has to reach the build environment.
//!
//! Two modes, in precedence order:
//!
//! 1. `WORKER_GIT_SSH_KEY` names a key — use exactly that one and generate
//!    nothing. This is the zero-touch path: the credential is provisioned with
//!    the machine, so deployment needs no follow-up action.
//! 2. Otherwise the worker generates an ed25519 keypair under `<data_dir>/ssh`
//!    on first start and logs the **public** half. Onboarding then costs one
//!    manual step: add that public key to the GitHub account. This is the
//!    documented default because a public key can only ever be attached to one
//!    GitHub account while an account may hold many — so N workers each holding
//!    their own key is the shape GitHub wants, not a workaround, and no private
//!    key is ever copied between machines.
//!
//! Generation happens *only* when no key was provided: a second, unused keypair
//! would make "which key is actually in use?" ambiguous at exactly the moment
//! someone is debugging an authentication failure.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::config::Config;

/// Directory the staged credential lives in — on the worker only.
///
/// `makechrootpkg` downloads sources **outside** the chroot: its
/// `download_sources()` runs `makepkg --verifysource` as the build user on the
/// worker, using the chroot's `makepkg.conf` but not its filesystem. So this is
/// where the key must be readable, and deliberately the *only* place it is.
///
/// It is never bind-mounted into the chroot. Everything a PKGBUILD executes —
/// `prepare`, `build`, `package` — runs in there, and a hostile or merely
/// careless one could read and exfiltrate any credential within reach. Sources
/// are already fetched by the time the chroot is entered, so exposing it there
/// would buy nothing.
///
/// The path is stable across jobs: `GIT_SSH_COMMAND` is written into the *base*
/// chroot's `makepkg.conf` when that chroot is created, and every later build
/// reads a copy of it, so a per-job path would be stale for every job after the
/// first.
pub fn secrets_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("secrets")
}
/// Key filename within that directory.
pub const KEY_FILE: &str = "id_ed25519";
/// `known_hosts` filename within that directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";

/// A credential staged for a build: the `GIT_SSH_COMMAND` that uses it.
///
/// Only the command is carried — the staged key path lives at the fixed
/// [`KEY_FILE`] slot the worker's ssh-agent loads (see [`stage_for_job`]), and
/// the command itself must never name it (the build user cannot read the file;
/// see [`git_ssh_command`]).
#[derive(Debug, Clone)]
pub struct StagedCredential {
    pub git_ssh_command: String,
}

/// Which SSH key a build should use, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Explicitly configured by the operator; never generated or overwritten.
    Provided(PathBuf),
    /// Worker-generated fallback, created on first start if absent.
    Generated(PathBuf),
}

impl KeySource {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Provided(p) | Self::Generated(p) => p,
        }
    }
}

/// Decide which key this worker builds with.
#[must_use]
pub fn resolve(cfg: &Config) -> KeySource {
    match &cfg.git_ssh_key {
        Some(path) => KeySource::Provided(path.clone()),
        None => KeySource::Generated(cfg.core.data_dir.join("ssh").join(KEY_FILE)),
    }
}

/// Public-key path for a private key, by OpenSSH's `.pub` convention.
#[must_use]
pub fn public_key_path(private: &Path) -> PathBuf {
    let mut name = private.as_os_str().to_os_string();
    name.push(".pub");
    PathBuf::from(name)
}

/// Ensure a generated key exists, creating it on first start.
///
/// A [`KeySource::Provided`] key is never touched — if it is missing that is an
/// operator error, and silently generating a different key in its place would
/// produce authentication failures that look like a server-side problem.
///
/// Returns the public key text when one is available, for logging.
pub async fn ensure(source: &KeySource) -> Result<Option<String>> {
    let path = source.path();
    match source {
        KeySource::Provided(_) => {
            if !path.exists() {
                anyhow::bail!(
                    "WORKER_GIT_SSH_KEY points at {}, which does not exist",
                    path.display()
                );
            }
            Ok(None)
        }
        KeySource::Generated(_) => {
            if !path.exists() {
                generate(path).await?;
            }
            Ok(std::fs::read_to_string(public_key_path(path)).ok())
        }
    }
}

/// Generate an ed25519 keypair with `ssh-keygen`.
///
/// Shelling out rather than pulling in an SSH key library keeps the format
/// question with the tool that defines it, and matches how the rest of this
/// crate drives `devtools`.
async fn generate(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating key dir {}", dir.display()))?;
    }
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-C", "aurcache-worker", "-f"])
        .arg(path)
        .status()
        .await
        .context("running ssh-keygen (is openssh installed?)")?;
    anyhow::ensure!(status.success(), "ssh-keygen failed with {status}");
    Ok(())
}

/// The `GIT_SSH_COMMAND` a build should use.
///
/// The key is **not** named here, and `IdentitiesOnly` is deliberately absent:
/// authentication goes through the ssh-agent the worker runs, reached via
/// `SSH_AUTH_SOCK`. The build user cannot read the key file at all — it belongs
/// to the worker's user and is listed in the sandbox's protected paths — so a
/// `-i` pointing at it would simply fail.
///
/// This is what makes the credential survive the uid split: the agent lets a
/// build *use* the key without ever being able to read it. A hostile PKGBUILD
/// can still ask the agent to authenticate during its own build (agent
/// hijacking is inherent and accepted), but it cannot obtain the key material,
/// so nothing it steals outlives the build.
#[must_use]
pub fn git_ssh_command(known_hosts: Option<&str>) -> String {
    let mut cmd = String::from("ssh");
    match known_hosts {
        Some(path) => cmd.push_str(&format!(" -o UserKnownHostsFile={path}")),
        // Without a known_hosts file ssh would prompt, which hangs a
        // non-interactive build forever; accept-new records on first contact
        // and still fails on a *changed* key.
        None => cmd.push_str(" -o StrictHostKeyChecking=accept-new"),
    }
    cmd
}

/// Append worker-local exports to the server-rendered `makepkg.conf`.
///
/// `makepkg.conf` is sourced by `makepkg`, so an `export` here reaches `git`
/// without `makechrootpkg` having to forward environment variables. Keeping it
/// worker-side means credentials never enter the server's job configuration.
///
/// The export is **guarded on the agent socket being present**, because one
/// file serves two environments: `download_sources()` runs on the worker, where
/// `SSH_AUTH_SOCK` is set and the agent is reachable, while the same
/// `makepkg.conf` is also read inside the chroot, where it deliberately is not.
/// Guarded, the export simply does not apply in there and ordinary git
/// operations inside the chroot behave as if AURCache had never touched the
/// config.
#[must_use]
pub fn augment_makepkg_conf(base: &str, credential: Option<&StagedCredential>) -> String {
    let Some(cred) = credential else {
        return base.to_string();
    };
    let mut out = base.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Added by aurcache-worker: build credentials (see credentials.rs).\n");
    out.push_str("# Guarded on the agent socket so ordinary git operations still work inside\n");
    out.push_str("# the chroot, where no agent is reachable: the export applies on the worker\n");
    out.push_str("# (which is where makepkg fetches sources) and nowhere else.\n");
    out.push_str(&format!(
        "if [ -n \"${{SSH_AUTH_SOCK:-}}\" ] && [ -S \"$SSH_AUTH_SOCK\" ]; then\n    \
         export GIT_SSH_COMMAND=\"{}\"\nfi\n",
        cred.git_ssh_command
    ));
    out
}

/// Parse `WORKER_BIND_MOUNTS` (`host:chroot,host:chroot`) into pairs.
///
/// Entries without exactly one `:` are dropped rather than guessed at: a
/// half-parsed bind mount would silently expose the wrong path into a build.
#[must_use]
pub fn parse_bind_mounts(raw: &str) -> Vec<(PathBuf, PathBuf)> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (host, chroot) = entry.split_once(':')?;
            let (host, chroot) = (host.trim(), chroot.trim());
            if host.is_empty() || chroot.is_empty() || chroot.contains(':') {
                return None;
            }
            Some((PathBuf::from(host), PathBuf::from(chroot)))
        })
        .collect()
}

/// Stage the build credentials for one job into `dir`, returning the
/// `GIT_SSH_COMMAND` to export.
///
/// The key is **copied** rather than bind-mounted from wherever the operator
/// put it: OpenSSH refuses a group- or world-readable private key, and a
/// docker-secret or root-owned mount would not be readable by the unprivileged
/// build user. Copying normalises owner and mode to this process's user, which
/// is the same user `makechrootpkg` runs `makepkg` as.
///
/// Read at job time rather than worker start, so replacing the key on the host
/// takes effect on the next build with no restart. The destination is shared by
/// every job (see [`secrets_dir`]) and simply overwritten each time.
pub fn stage_for_job(cfg: &Config, dir: &Path) -> Result<Option<StagedCredential>> {
    let source = resolve(cfg);
    let key = source.path();
    if !key.exists() {
        // A `Provided` key is an operator error, mirroring `ensure`; a
        // `Generated` key simply may not exist yet.
        if matches!(source, KeySource::Provided(_)) {
            anyhow::bail!(
                "WORKER_GIT_SSH_KEY points at {}, which does not exist",
                key.display()
            );
        }
        return Ok(None);
    }

    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating secrets dir {}", dir.display()))?;
    let staged_key = dir.join(KEY_FILE);
    std::fs::copy(key, &staged_key)
        .with_context(|| format!("copying {} into the job workspace", key.display()))?;
    set_private_mode(&staged_key)?;

    let known_hosts = match &cfg.ssh_known_hosts {
        Some(src) if src.exists() => {
            let staged = dir.join(KNOWN_HOSTS_FILE);
            std::fs::copy(src, &staged).context("copying known_hosts")?;
            Some(staged.display().to_string())
        }
        _ => None,
    };

    Ok(Some(StagedCredential {
        git_ssh_command: git_ssh_command(known_hosts.as_deref()),
    }))
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting 0600 on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(key: Option<&str>, data_dir: &str) -> Config {
        let mut cfg = Config::from_env();
        cfg.git_ssh_key = key.map(PathBuf::from);
        cfg.core.data_dir = PathBuf::from(data_dir);
        cfg.ssh_known_hosts = None;
        cfg
    }

    #[test]
    fn an_explicit_key_takes_precedence_and_is_never_generated() {
        let source = resolve(&cfg_with(Some("/run/secrets/epic"), "/var/lib/w"));
        assert_eq!(
            source,
            KeySource::Provided(PathBuf::from("/run/secrets/epic"))
        );
    }

    #[test]
    fn without_an_explicit_key_one_is_generated_under_data_dir() {
        let source = resolve(&cfg_with(None, "/var/lib/w"));
        assert_eq!(
            source,
            KeySource::Generated(PathBuf::from("/var/lib/w/ssh/id_ed25519"))
        );
    }

    /// A missing *provided* key must be an error, not a silent fallback to a
    /// generated one — the resulting auth failures would look server-side.
    #[tokio::test]
    async fn a_missing_provided_key_is_an_error() {
        let source = KeySource::Provided(PathBuf::from("/nonexistent/key"));
        assert!(ensure(&source).await.is_err());
    }

    #[test]
    fn public_key_path_follows_openssh_convention() {
        assert_eq!(
            public_key_path(Path::new("/k/id_ed25519")),
            PathBuf::from("/k/id_ed25519.pub")
        );
    }

    /// Authentication goes through the agent, so the command must name no key
    /// and must not set `IdentitiesOnly`, which would suppress agent identities
    /// and break authenticated fetches outright.
    #[test]
    fn git_ssh_command_uses_the_agent_and_sets_host_policy() {
        let with_hosts = git_ssh_command(Some("/staged/known_hosts"));
        assert!(!with_hosts.contains("-i "), "must not name a key file");
        assert!(!with_hosts.contains("IdentitiesOnly"));
        assert!(with_hosts.contains("UserKnownHostsFile=/staged/known_hosts"));
        // No known_hosts must not leave ssh prompting, which would hang a build.
        let without = git_ssh_command(None);
        assert!(without.contains("StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn makepkg_conf_gains_the_export_only_when_a_credential_exists() {
        let base = "PKGDEST=/output";
        assert_eq!(augment_makepkg_conf(base, None), base);

        let cred = StagedCredential {
            git_ssh_command: "ssh -o StrictHostKeyChecking=accept-new".to_string(),
        };
        let augmented = augment_makepkg_conf(base, Some(&cred));
        assert!(augmented.starts_with("PKGDEST=/output\n"));
        assert!(
            augmented
                .contains("export GIT_SSH_COMMAND=\"ssh -o StrictHostKeyChecking=accept-new\"")
        );
    }

    /// The export must be guarded on the agent socket. The same `makepkg.conf`
    /// is read on the worker (where the agent is reachable) and inside the
    /// chroot (where it is not); unguarded, ordinary git operations in a
    /// PKGBUILD would be pointed at a socket that does not exist there.
    #[test]
    fn the_export_is_guarded_so_it_does_not_apply_in_the_chroot() {
        let cred = StagedCredential {
            git_ssh_command: "ssh -o StrictHostKeyChecking=accept-new".to_string(),
        };
        let conf = augment_makepkg_conf("PKGDEST=/output", Some(&cred));

        assert!(conf.contains("SSH_AUTH_SOCK"));
        assert!(conf.trim_end().ends_with("fi"));
        // The guard must wrap the export, not sit beside it.
        let guard = conf.find("if [ -n").expect("guard present");
        let export = conf.find("export GIT_SSH_COMMAND").expect("export present");
        assert!(guard < export, "export must be inside the guard");
        // The key path must not leak into a config the chroot also reads.
        assert!(!conf.contains("/staged/id_ed25519"));
    }

    #[test]
    fn bind_mounts_parse_pairs_and_drop_malformed_entries() {
        let parsed = parse_bind_mounts("/host/a:/chroot/a, /host/b:/chroot/b");
        assert_eq!(
            parsed,
            vec![
                (PathBuf::from("/host/a"), PathBuf::from("/chroot/a")),
                (PathBuf::from("/host/b"), PathBuf::from("/chroot/b")),
            ]
        );
        // Guessing at a half-specified mount could expose the wrong path.
        assert!(parse_bind_mounts("/host/a").is_empty());
        assert!(parse_bind_mounts(":/chroot").is_empty());
        assert!(parse_bind_mounts("/host:").is_empty());
        assert!(parse_bind_mounts("/a:/b:/c").is_empty());
        assert!(parse_bind_mounts("").is_empty());
    }

    #[test]
    fn staging_copies_the_key_with_owner_only_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("src-key");
        std::fs::write(&key, "PRIVATE").unwrap();
        // World-readable at the source, as a bind-mounted docker secret may be.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let dest = secrets_dir(tmp.path());
        let cfg = cfg_with(key.to_str(), tmp.path().to_str().unwrap());
        let cred = stage_for_job(&cfg, &dest).unwrap().expect("staged");

        // The key is still staged at a stable path — that is where the worker's
        // ssh-agent loads it from — but the command must not name it: the build
        // user cannot read it, and authentication goes through the agent.
        let staged = dest.join(KEY_FILE);
        assert!(!cred.git_ssh_command.contains(&staged.display().to_string()));
        assert!(!cred.git_ssh_command.contains("-i "));
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "PRIVATE");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged).unwrap().permissions().mode();
            // ssh refuses a group/world-readable private key.
            assert_eq!(mode & 0o077, 0, "staged key must be owner-only");
        }
    }
}
