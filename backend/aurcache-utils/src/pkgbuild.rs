use std::path::{Path, PathBuf};
use std::process::Command;

use alpm_pkgbuild::bridge::BridgeOutput;
use alpm_srcinfo::SourceInfoV1;
use anyhow::Context;

/// Fix known-erroneous source URLs in PKGBUILD or .SRCINFO content that
/// makepkg accepts but `alpm-srcinfo` rejects.
///
/// Currently only fixes `?signed/` → `?signed` (trailing slash after the
/// signed flag in git VCS source URLs). Only affects libaegis and h2o-git.
pub fn fix_source_urls(content: &str) -> String {
    content.replace("?signed/", "?signed")
}

/// How a PKGBUILD is parsed: `alpm-pkgbuild-bridge`, confined by
/// `aurcache-sandbox`.
///
/// The bridge parses a PKGBUILD by *sourcing* it, so every parse runs
/// attacker-supplied bash in the process that owns the package database and
/// the repository. `SourceInfoV1::from_pkgbuild` would find the bridge through
/// `PATH` and hand it this process's environment, so confinement would hinge on
/// a wrapper shadowing it there -- and a missing wrapper would parse unconfined
/// without a word. This runs both by absolute path instead: without the
/// sandbox, parsing fails rather than proceeding.
///
/// Four defences, because Landlock's filesystem policy alone closes only one of
/// them:
///
///   1. Nothing is writable but `/dev/null`. The working directory is the
///      PKGBUILD's own, readable and no more: sourcing a recipe writes nothing,
///      and anything writable would be space a PKGBUILD could fill -- the
///      server's disk, with the database and the repository on it. For a git
///      source that directory is also the checkout that is archived and sent
///      to the workers, which a writable parse could rewrite after its
///      metadata was read. The one casualty is a top-level heredoc too large
///      for a pipe, which bash spills to `$TMPDIR`; that statement fails and
///      the parse goes on.
///   2. `protected` is unreadable: the server's working directory, which holds
///      `./db`, `./repo` and `./data/ca`, and `/etc/aurcache`, which holds the
///      environment file the database password and OAuth secret come from.
///   3. The environment is replaced. Landlock cannot protect a secret already
///      in memory, and this process's environment carries `DB_PWD` and
///      `SECRET_KEY`, which a PKGBUILD reads without touching a file.
///   4. [`EXTRAS`] cuts the parse off from the processes around it, and from
///      TCP -- though not from the network as such; see there.
#[derive(Debug, Clone)]
pub(crate) struct Bridge {
    sandbox: PathBuf,
    script: PathBuf,
    protected: Vec<PathBuf>,
    /// Whether the parse may use the network; see [`EXTRAS`].
    network: bool,
}

/// Paths kept unreadable in addition to the server's working directory.
///
/// The native packages put the server's environment file here; the parse needs
/// nothing from it. A path that does not exist costs nothing to name.
const PROTECTED_PATHS: [&str; 1] = ["/etc/aurcache"];

/// Restrictions the sandbox applies beyond its filesystem policy.
///
/// A parse needs neither, so both are free to apply. `--isolate-ipc` needs
/// Linux 6.12, the server's documented floor, and the sandbox refuses to run if
/// it cannot enforce either -- degrading instead would leave the policy's
/// strength depending on the host, unremarked.
///
/// `--no-net` is **not** an egress boundary: the Landlock our floor gives us
/// knows only TCP, so UDP, DNS and QUIC remain open, and `perl` is in the
/// server image. It raises the cost of phoning home rather than preventing it,
/// which is worth having only because the defences above leave so little worth
/// sending: no database, no repository, no `server.env`, and an environment
/// with no secrets in it. A host firewall is the control that actually stops
/// egress.
///
/// Landlock ABI 10 does add UDP (`BIND_UDP`, `CONNECT_SEND_UDP`), but it is out
/// of reach twice over: it needs a kernel far newer than the 6.12 floor these
/// deployments run, and `landlock` 0.4.7 tops out at ABI V9, so there is no API
/// for it that keeps the crate's compatibility checks. Revisit when both move;
/// raw and ICMP sockets would still be outside Landlock's model, so only a
/// seccomp filter denying `AF_INET` sockets closes the channel completely.
const EXTRAS: [&str; 2] = ["--no-net", "--isolate-ipc"];

/// The subset applied when the `parse_network` setting is on.
///
/// Around 0.2% of AUR packages compute `pkgver` while being sourced, with
/// `pkgver=$(curl -s https://api.github.com/...)` or `git ls-remote`, and parse
/// to nothing when TCP is denied. Isolation from the processes around the parse
/// is not what those packages need, so it stays.
const EXTRAS_WITH_NETWORK: [&str; 1] = ["--isolate-ipc"];

impl Bridge {
    /// Resolve the installed layout.
    ///
    /// The defaults are the native packages' paths; the container images
    /// install under `/usr/local` and set `AURCACHE_SANDBOX` and
    /// `AURCACHE_PKGBUILD_BRIDGE` accordingly.
    pub(crate) fn from_env(network: bool) -> anyhow::Result<Self> {
        let path_from = |key: &str, default: &str| {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        let cwd = std::env::current_dir()
            .context("cannot determine the directory to protect from PKGBUILD parsing")?;
        // `AURCACHE_PROTECTED_DIR` adds to the defaults rather than replacing
        // them, so a deployment naming one more directory cannot accidentally
        // expose the server's own.
        let mut protected = vec![cwd.clone()];
        protected.extend(PROTECTED_PATHS.map(PathBuf::from));
        if let Some(dirs) = std::env::var_os("AURCACHE_PROTECTED_DIR").filter(|v| !v.is_empty()) {
            protected.extend(std::env::split_paths(&dirs).map(|dir| cwd.join(dir)));
        }
        protected.dedup();

        Ok(Self {
            network,
            sandbox: path_from("AURCACHE_SANDBOX", "/usr/bin/aurcache-sandbox"),
            script: path_from("AURCACHE_PKGBUILD_BRIDGE", "/usr/bin/alpm-pkgbuild-bridge"),
            protected,
        })
    }

    /// Whether both executables exist, for tests that need a real parse.
    #[cfg(test)]
    pub(crate) fn is_installed(&self) -> bool {
        self.sandbox.is_file() && self.script.is_file()
    }

    /// The confined invocation that parses the PKGBUILD at `pkgbuild`.
    fn command(&self, pkgbuild: &Path) -> anyhow::Result<Command> {
        // Absolute, because the sandbox resolves `--read` after the working
        // directory has already moved there.
        let pkgbuild = std::path::absolute(pkgbuild)
            .with_context(|| format!("cannot resolve {}", pkgbuild.display()))?;
        let (Some(dir), Some(file)) = (pkgbuild.parent(), pkgbuild.file_name()) else {
            anyhow::bail!("{} does not name a file", pkgbuild.display());
        };

        let mut command = Command::new(&self.sandbox);
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .env("LANG", "C.UTF-8")
            .current_dir(dir)
            .args(if self.network {
                &EXTRAS_WITH_NETWORK[..]
            } else {
                &EXTRAS[..]
            })
            // Read-only: see the first defence above. Granted explicitly because
            // the directory may sit under a protected one -- the server's own
            // working directory holds the checkouts.
            .arg("--read")
            .arg(dir);
        for protected in &self.protected {
            command.arg("--read-except").arg(protected);
        }
        command.arg("--").arg(&self.script).arg(file);
        Ok(command)
    }

    /// Parse the PKGBUILD at `pkgbuild`.
    pub(crate) fn parse(&self, pkgbuild: &Path) -> anyhow::Result<SourceInfoV1> {
        let output = self.command(pkgbuild)?.output().with_context(|| {
            format!(
                "cannot run {} to parse {}",
                self.sandbox.display(),
                pkgbuild.display()
            )
        })?;
        if !output.status.success() {
            // The bridge reports some errors on stdout, the sandbox on stderr.
            anyhow::bail!(
                "parsing {} failed ({}): {} {}",
                pkgbuild.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim(),
            );
        }
        let stdout = std::str::from_utf8(&output.stdout)
            .context("alpm-pkgbuild-bridge printed invalid UTF-8")?;
        let bridge = BridgeOutput::from_script_output(stdout)?;
        Ok(bridge.try_into()?)
    }
}

/// Commands that need the network, in the shapes a PKGBUILD uses to compute a
/// version while it is being sourced.
const NETWORK_COMMANDS: [&str; 6] = [
    "curl",
    "wget",
    "aria2c",
    "git ls-remote",
    "git fetch",
    "svn",
];

/// Say so when a parse failed because the PKGBUILD wanted the network.
///
/// A denied `curl` leaves `pkgver` empty, and the parse then fails somewhere
/// else entirely -- on a missing version, not on the connection. Without this
/// the operator sees a version error and has no way to reach the setting that
/// fixes it. Only the code that actually runs is considered: everything from
/// the first function definition on is not executed by sourcing the file.
fn explain_network_need(error: anyhow::Error, content: &str, network: bool) -> anyhow::Error {
    if network {
        return error;
    }
    // Borrowed lines, not a joined `String` copy: the question is which
    // network command the top-level lines name, and the order asked is
    // commands-first (the first listed command found anywhere wins).
    let top_level: Vec<&str> = content
        .lines()
        .take_while(|line| !line.contains("() {") && !line.trim_end().ends_with("()"))
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    let Some(command) = NETWORK_COMMANDS
        .into_iter()
        .find(|command| top_level.iter().any(|line| line.contains(command)))
    else {
        return error;
    };
    error.context(format!(
        "this PKGBUILD runs `{command}` while it is parsed, and parsing is \
         confined without network access; set `parse_network` if this source \
         is trusted"
    ))
}

/// Parse a PKGBUILD file, applying workarounds for common issues found in
/// real-world PKGBUILDs that makepkg accepts but `alpm-srcinfo` rejects.
pub fn parse_pkgbuild(path: &Path, network: bool) -> anyhow::Result<SourceInfoV1> {
    let bridge = Bridge::from_env(network)?;
    let error = match bridge.parse(path) {
        Ok(info) => return Ok(info),
        Err(error) => error,
    };

    let raw = std::fs::read_to_string(path)?;
    let fixed = fix_source_urls(&raw);
    if fixed == raw {
        return Err(explain_network_need(error, &raw, network));
    }

    let dir = tempfile::tempdir()?;
    let fixed_path = dir.path().join("PKGBUILD");
    std::fs::write(&fixed_path, &fixed)?;
    let result = bridge.parse(&fixed_path)?;
    dir.close()?;
    Ok(result)
}

/// Parse PKGBUILD content held in memory, applying the same workarounds as
/// [`parse_pkgbuild`]. Since the bridge is a bash script that needs a real
/// filesystem path to `source` the PKGBUILD, the content is written to a
/// short-lived temp file that's removed again as soon as parsing finishes - no
/// other part of this function touches disk.
pub fn parse_pkgbuild_content(content: &str, network: bool) -> anyhow::Result<SourceInfoV1> {
    let bridge = Bridge::from_env(network)?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("PKGBUILD");

    std::fs::write(&path, content)?;
    let result = bridge.parse(&path).or_else(|error| {
        let fixed = fix_source_urls(content);
        if fixed == content {
            return Err(explain_network_need(error, content, network));
        }
        std::fs::write(&path, &fixed)?;
        bridge.parse(&path)
    });

    dir.close()?;
    result
}

/// Render PKGBUILD content as `.SRCINFO`.
pub fn pkgbuild_to_srcinfo(content: &str, network: bool) -> anyhow::Result<String> {
    Ok(parse_pkgbuild_content(content, network)?.as_srcinfo())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The bridge and the sandbox are installed by the images, not by a plain
    /// `cargo test`, so tests that parse a PKGBUILD skip without them. CI sets
    /// `AURCACHE_SANDBOX` and `AURCACHE_PKGBUILD_BRIDGE`; there a missing
    /// executable is a misconfigured job, not a reason to skip.
    pub(crate) fn bridge_available() -> bool {
        let available = Bridge::from_env(false).is_ok_and(|bridge| bridge.is_installed());
        assert!(
            available || std::env::var_os("CI").is_none(),
            "AURCACHE_SANDBOX and AURCACHE_PKGBUILD_BRIDGE must name installed executables in CI"
        );
        available
    }

    fn bridge_protecting(protected: &Path) -> Bridge {
        Bridge {
            protected: vec![protected.to_path_buf()],
            ..Bridge::from_env(false).unwrap()
        }
    }

    fn fake_bridge() -> Bridge {
        Bridge {
            sandbox: PathBuf::from("/opt/sandbox"),
            script: PathBuf::from("/opt/bridge"),
            protected: vec![
                PathBuf::from("/srv/aurcache"),
                PathBuf::from("/etc/aurcache"),
            ],
            network: false,
        }
    }

    /// The invocation carries the policy: every protected directory, the
    /// PKGBUILD's own directory readable and nothing writable, the kernel
    /// restrictions, and the script by absolute path rather than whatever
    /// `PATH` finds.
    #[test]
    fn command_confines_the_bridge_to_the_pkgbuild_directory() {
        let command = fake_bridge()
            .command(Path::new("/work/demo/PKGBUILD"))
            .unwrap();

        assert_eq!(command.get_program(), "/opt/sandbox");
        assert_eq!(command.get_current_dir(), Some(Path::new("/work/demo")));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--no-net",
                "--isolate-ipc",
                "--read",
                "/work/demo",
                "--read-except",
                "/srv/aurcache",
                "--read-except",
                "/etc/aurcache",
                "--",
                "/opt/bridge",
                "PKGBUILD"
            ]
        );
        let envs: Vec<_> = command.get_envs().map(|(key, _)| key).collect();
        assert_eq!(envs, ["HOME", "LANG", "PATH"]);
    }

    /// A relative path must not reach the sandbox as-is: it resolves `--read`
    /// from the PKGBUILD's directory, where `demo` would name something else.
    #[test]
    fn command_makes_the_pkgbuild_directory_absolute() {
        let command = fake_bridge().command(Path::new("demo/PKGBUILD")).unwrap();
        let dir = command.get_current_dir().unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with("demo"));
    }

    /// Missing the sandbox is an error, never an unconfined parse.
    #[test]
    fn a_missing_sandbox_fails_the_parse() {
        let dir = tempfile::tempdir().unwrap();
        let pkgbuild = dir.path().join("PKGBUILD");
        std::fs::write(&pkgbuild, "pkgname=demo\npkgver=1\npkgrel=1\narch=(any)\n").unwrap();
        let bridge = Bridge {
            sandbox: dir.path().join("no-such-sandbox"),
            ..fake_bridge()
        };
        let error = bridge.parse(&pkgbuild).unwrap_err();
        assert!(
            format!("{error:#}").contains("no-such-sandbox"),
            "{error:#}"
        );
    }

    /// The same parse with no sandbox at all, as `SourceInfoV1::from_pkgbuild`
    /// would do it.
    ///
    /// Every assertion about a denial is paired with one of these, because an
    /// allow-list fails open: that a fixture did not read a file means nothing
    /// unless the same fixture demonstrably reads it unconfined. A policy
    /// widened by accident turns this leg's assertion red.
    fn parse_unconfined(pkgbuild: &Path) -> String {
        let output = Command::new(Bridge::from_env(false).unwrap().script)
            .current_dir(pkgbuild.parent().unwrap())
            .arg(pkgbuild.file_name().unwrap())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// End to end against the real sandbox: a PKGBUILD can neither read the
    /// protected directory nor write anywhere, its own directory included --
    /// and it still parses from inside a protected directory, which is where
    /// the server keeps its checkouts.
    #[test]
    fn a_parse_cannot_read_protected_files_or_write_anywhere() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let protected = tempfile::tempdir().unwrap();
        std::fs::write(protected.path().join("secret"), "2").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let marker = elsewhere.path().join("marker");
        let work = protected.path().join("checkout");
        std::fs::create_dir(&work).unwrap();
        let own_marker = work.join("marker");
        let pkgbuild = work.join("PKGBUILD");
        std::fs::write(
            &pkgbuild,
            format!(
                "pkgname=demo\n\
                 pkgver=$(cat {secret} 2>/dev/null || echo 1)\n\
                 pkgrel=1\n\
                 arch=(any)\n\
                 $(echo touched > {marker} 2>/dev/null)\n\
                 $(echo touched > {own_marker} 2>/dev/null)\n\
                 package() {{ :; }}\n",
                secret = protected.path().join("secret").display(),
                marker = marker.display(),
                own_marker = own_marker.display(),
            ),
        )
        .unwrap();

        let unconfined = parse_unconfined(&pkgbuild);
        assert!(
            unconfined.contains("pkgver \"2\""),
            "fixture proves nothing: it did not read the secret unconfined: {unconfined}"
        );
        assert!(marker.exists(), "fixture did not write the marker either");
        assert!(own_marker.exists(), "fixture did not write beside itself");
        std::fs::remove_file(&marker).unwrap();
        std::fs::remove_file(&own_marker).unwrap();

        let info = bridge_protecting(protected.path())
            .parse(&pkgbuild)
            .unwrap();

        assert_eq!(
            info.base.version.to_string(),
            "1-1",
            "read the protected file"
        );
        assert!(!marker.exists(), "wrote outside the PKGBUILD's directory");
        assert!(!own_marker.exists(), "wrote in the PKGBUILD's directory");
    }

    /// With `parse_network` on, the TCP denial is dropped -- and only that:
    /// the parse still writes nothing and stays isolated from the processes
    /// around it.
    #[test]
    fn allowing_the_network_drops_only_the_tcp_denial() {
        let bridge = Bridge {
            network: true,
            ..fake_bridge()
        };
        let command = bridge.command(Path::new("/work/demo/PKGBUILD")).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        assert!(!args.contains(&"--no-net".to_string()), "{args:?}");
        assert!(args.contains(&"--isolate-ipc".to_string()), "{args:?}");
        assert!(args.contains(&"--read-except".to_string()), "{args:?}");
    }

    /// A PKGBUILD that fetches its version while being sourced fails on the
    /// missing version, not on the denied connection, so the error has to name
    /// the cause and the setting that lifts it.
    #[test]
    fn a_failed_parse_names_the_setting_when_the_pkgbuild_wants_the_network() {
        let pkgbuild = "pkgname=demo\n\
                        pkgver=$(curl -s https://api.example.com/latest)\n\
                        package() { :; }\n";
        let explained = explain_network_need(anyhow::anyhow!("missing pkgver"), pkgbuild, false);
        let message = format!("{explained:#}");
        assert!(message.contains("curl"), "{message}");
        assert!(message.contains("parse_network"), "{message}");

        // With the setting already on, the failure is something else and must
        // not be dressed up as this one.
        let unchanged = explain_network_need(anyhow::anyhow!("missing pkgver"), pkgbuild, true);
        assert_eq!(format!("{unchanged:#}"), "missing pkgver");
    }

    /// The hint is only for code that runs: `curl` inside `package()` is not
    /// executed by sourcing the file, so it explains nothing.
    #[test]
    fn the_hint_ignores_network_use_inside_functions() {
        let pkgbuild = "pkgname=demo\n\
                        pkgver=1\n\
                        package() {\n  curl -s https://api.example.com/x\n}\n";
        let error = explain_network_need(anyhow::anyhow!("boom"), pkgbuild, false);
        assert_eq!(format!("{error:#}"), "boom");
    }

    /// A parse has no business connecting anywhere, and `--no-net` is what
    /// stops it exfiltrating what it did manage to read. Landlock denies the
    /// connection with `EACCES`, which bash reports as "Permission denied" --
    /// distinct from the "Connection refused" an unconfined attempt gets from a
    /// closed local port.
    #[test]
    fn a_parse_cannot_open_a_tcp_connection() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let work = tempfile::tempdir().unwrap();
        let pkgbuild = work.path().join("PKGBUILD");
        std::fs::write(
            &pkgbuild,
            "pkgname=demo\n\
             pkgver=$( (exec 3<>/dev/tcp/127.0.0.1/1) 2>&1 \
                 | grep -q 'Permission denied' && echo 1 || echo 2)\n\
             pkgrel=1\n\
             arch=(any)\n\
             package() { :; }\n",
        )
        .unwrap();

        let unconfined = parse_unconfined(&pkgbuild);
        assert!(
            unconfined.contains("pkgver \"2\""),
            "fixture proves nothing: the connection was refused rather than denied: {unconfined}"
        );

        let info = Bridge::from_env(false).unwrap().parse(&pkgbuild).unwrap();

        assert_eq!(info.base.version.to_string(), "1-1", "TCP was reachable");
    }

    #[test]
    fn renders_srcinfo_from_pkgbuild_content() {
        if !bridge_available() {
            eprintln!("skipping: aurcache-sandbox or alpm-pkgbuild-bridge not installed");
            return;
        }
        let srcinfo = pkgbuild_to_srcinfo(
            "pkgname=demo\npkgver=1.2\npkgrel=3\narch=(any)\npackage() { :; }\n",
            false,
        )
        .unwrap();
        assert!(srcinfo.contains("pkgbase = demo"), "{srcinfo}");
        assert!(srcinfo.contains("pkgver = 1.2"), "{srcinfo}");
    }
}
