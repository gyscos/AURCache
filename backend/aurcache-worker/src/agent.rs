//! The build credential's ssh-agent.
//!
//! A build needs to *use* the worker's SSH key -- some PKGBUILDs fetch from
//! private repositories -- while never being able to read it. The key belongs
//! to the worker's user; builds run as a different, unprivileged one. An agent
//! is what bridges that: it holds the key in this process's own child and hands
//! out signatures over a socket, so a build authenticates without the key
//! material ever being within its reach.
//!
//! Started here, by the worker, rather than by a shell script in the container
//! entrypoints. That was the arrangement before, and it meant the native
//! package had no agent at all -- the script lived under `docker/` and was
//! never packaged, so a natively installed worker could not fetch an
//! authenticated source however it was configured. It also had to run *before*
//! the worker, which is before the key it wanted to load necessarily existed,
//! so a fresh deployment silently started without one and needed a restart to
//! pick it up. Starting it here removes both: one mechanism, after the key is
//! ensured, on every platform.
//!
//! ## What a build can and cannot do with it
//!
//! It can ask the agent to authenticate, for anything, for as long as the build
//! runs -- no host restrictions are applied, because a source may legitimately
//! live on any host. That is accepted. What it cannot do is read the key, so
//! nothing it captures outlives the build.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};

/// Directory the socket lives in, under the worker's data directory.
///
/// Deliberately not `/run`: that is root-owned on a host and the worker is not,
/// so putting it there needs `sudo` on the native path and a mkdir in the
/// container entrypoint -- the split this module exists to remove. The data
/// directory is already the worker's own on both.
const AGENT_DIR: &str = "agent";
const SOCKET_FILE: &str = "agent.sock";

/// The group builds run as, which must be able to reach the socket.
const BUILD_GROUP: &str = "aurbuild";

/// A running ssh-agent holding the build credential.
///
/// Killed when dropped, so the agent's lifetime is the worker's and no stray
/// agent survives a restart holding a key.
pub struct BuildAgent {
    socket: PathBuf,
    child: Child,
}

impl BuildAgent {
    /// The socket path, for `SSH_AUTH_SOCK` and for the chroot bind mount.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The directory to bind into a build chroot.
    ///
    /// The *directory* rather than the socket itself: a bind mount of a socket
    /// breaks when the agent is restarted and the inode is replaced, while the
    /// directory outlives it.
    #[must_use]
    pub fn bind_dir(&self) -> &Path {
        // `socket` is `<dir>/agent.sock`, so its parent is the directory.
        self.socket.parent().unwrap_or(&self.socket)
    }
}

impl Drop for BuildAgent {
    fn drop(&mut self) {
        self.child.start_kill().ok();
        std::fs::remove_file(&self.socket).ok();
    }
}

/// Start an agent holding `key`, and publish its socket to builds.
///
/// Returns `None` when there is no key to load, which is the ordinary case for
/// a worker that builds nothing needing authentication.
pub async fn start(data_dir: &Path, key: &Path) -> Result<Option<BuildAgent>> {
    if !key.exists() {
        return Ok(None);
    }

    let dir = data_dir.join(AGENT_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Traversable by the build group and nobody else: builds reach the socket
    // through it, and the key is not in here at all.
    set_group_and_mode(&dir, 0o750)?;

    let socket = dir.join(SOCKET_FILE);
    // A socket left by a previous run: ssh-agent refuses to bind over it.
    std::fs::remove_file(&socket).ok();

    // `-D` keeps it in the foreground as our child, so its lifetime is ours.
    // The alternative -- letting it daemonize and parsing the shell snippet it
    // prints -- leaves an agent behind whenever the worker dies badly.
    let child = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .kill_on_drop(true)
        .spawn()
        .context("starting ssh-agent")?;

    wait_for_socket(&socket).await?;
    set_group_and_mode(&socket, 0o660)?;

    let added = Command::new("ssh-add")
        .arg(key)
        .env("SSH_AUTH_SOCK", &socket)
        .output()
        .await
        .context("running ssh-add")?;
    if !added.status.success() {
        anyhow::bail!(
            "ssh-add refused the build credential: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        );
    }

    Ok(Some(BuildAgent { socket, child }))
}

/// ssh-agent creates its socket a moment after starting.
async fn wait_for_socket(socket: &Path) -> Result<()> {
    for _ in 0..100 {
        if socket.exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("ssh-agent did not create {} in time", socket.display())
}

/// Give the build group access, without granting it to everyone.
#[cfg(unix)]
fn set_group_and_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting {mode:o} on {}", path.display()))?;
    // Best-effort: a deployment without the build group still works, it just
    // runs builds as a user that can already reach the socket.
    if let Some(gid) = group_id(BUILD_GROUP) {
        chown_group(path, gid)
            .with_context(|| format!("giving {BUILD_GROUP} access to {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn group_id(name: &str) -> Option<u32> {
    // Read from the group database rather than shelling out to `id`, which
    // would be a process per call for one lookup.
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name).then(|| fields.nth(1)?.parse().ok())?
    })
}

#[cfg(unix)]
fn chown_group(path: &Path, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the
    // call; -1 leaves the owner unchanged.
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bind mount is the directory, not the socket: the socket's inode is
    /// replaced whenever the agent restarts, and a bind to the old one would
    /// survive as a broken mount.
    #[test]
    fn the_bind_target_is_the_directory() {
        let agent = PathBuf::from("/var/lib/aurcache-worker/agent/agent.sock");
        assert_eq!(
            agent.parent().unwrap(),
            Path::new("/var/lib/aurcache-worker/agent")
        );
    }

    /// A worker with no credential is the ordinary case and must not fail.
    #[tokio::test]
    async fn no_key_means_no_agent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(start(dir.path(), &missing).await.unwrap().is_none());
    }

    /// The group lookup reads the real database; an absent group is not an
    /// error, it just means nothing extra is granted.
    #[test]
    fn an_unknown_group_is_not_an_error() {
        assert!(group_id("definitely-not-a-real-group-name").is_none());
    }
}
