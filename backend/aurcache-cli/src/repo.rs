//! `aurcache-cli repo config` — the `pacman.conf` stanza for this instance.
//!
//! The last step of setting AURCache up is consuming it, and the documentation
//! can only print a stanza with `<server_ip>` left as an exercise. The CLI
//! already knows the address, because it is talking to the thing: the repo is
//! the same host on the mirror port. So this command prints the finished block
//! with nothing left to substitute.

use crate::url::{host_from_url, scheme_from_url};
use anyhow::{Result, anyhow};
use aurcache_common::ports::AURCACHE_MIRROR_PORT;
use serde::Serialize;

/// What `repo_init` names the repository, and therefore what pacman must call
/// it: the database files are `repo.db` / `repo.files`, so the section header
/// is not a free choice.
pub const DEFAULT_REPO_NAME: &str = "repo";

/// Matches the documented stanza. AURCache does not sign its packages, so a
/// stricter level would refuse everything it serves.
pub const DEFAULT_SIGLEVEL: &str = "Optional TrustAll";

#[derive(Debug, Clone, Serialize)]
pub struct RepoConfig {
    pub name: String,
    pub server: String,
    pub siglevel: String,
    /// The three lines as they go into `pacman.conf`, so a JSON consumer does
    /// not have to reassemble them and get the order or spacing wrong.
    pub block: String,
}

/// The `Server =` value for the repository published by the instance at
/// `api_url`.
///
/// `$arch` is pacman's own variable and is left for pacman to expand — one
/// stanza serves every architecture the instance builds for.
pub fn repo_server_url(api_url: &str, port: u16) -> Result<String> {
    let host = host_from_url(api_url)
        .ok_or_else(|| anyhow!("could not read a host out of the API URL `{api_url}`"))?;
    Ok(format!(
        "{}://{host}:{port}/$arch",
        scheme_from_url(api_url)
    ))
}

/// Build the stanza for the instance at `api_url`.
pub fn repo_config(
    api_url: &str,
    port: Option<u16>,
    name: Option<String>,
    siglevel: Option<String>,
) -> Result<RepoConfig> {
    let server = repo_server_url(api_url, port.unwrap_or(AURCACHE_MIRROR_PORT))?;
    let name = name.unwrap_or_else(|| DEFAULT_REPO_NAME.to_string());
    let siglevel = siglevel.unwrap_or_else(|| DEFAULT_SIGLEVEL.to_string());
    let block = format!("[{name}]\nSigLevel = {siglevel}\nServer = {server}\n");
    Ok(RepoConfig {
        name,
        server,
        siglevel,
        block,
    })
}

/// Print the stanza on stdout and the what-to-do-with-it on stderr.
///
/// Split that way on purpose: `aurcache-cli repo config >> pacman.conf` has to
/// append three usable lines and nothing else, so the instructions cannot share
/// the stream with them.
pub fn print_repo_config(config: &RepoConfig) {
    print!("{}", config.block);
    eprintln!();
    eprintln!("Append the above to /etc/pacman.conf, then refresh:");
    eprintln!("  aurcache-cli repo config | sudo tee -a /etc/pacman.conf");
    eprintln!("  sudo pacman -Sy");
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_REPO_NAME, repo_config, repo_server_url};

    #[test]
    fn the_repo_is_the_api_host_on_the_mirror_port() {
        let server = repo_server_url("http://192.168.1.10:8080/api", 8081).unwrap();
        assert_eq!(server, "http://192.168.1.10:8081/$arch");
    }

    /// A TLS-terminated API means a TLS-terminated repo on the same host; the
    /// alternative is emitting a URL that a reverse-proxied deployment cannot
    /// reach.
    #[test]
    fn the_scheme_follows_the_api_url() {
        let server = repo_server_url("https://aurcache.example.com/api", 8081).unwrap();
        assert_eq!(server, "https://aurcache.example.com:8081/$arch");
    }

    #[test]
    fn an_ipv6_host_stays_bracketed() {
        let server = repo_server_url("http://[::1]:8080/api", 8081).unwrap();
        assert_eq!(server, "http://[::1]:8081/$arch");
    }

    /// `$arch` belongs to pacman: one stanza has to serve every architecture
    /// the instance builds, so the CLI must not expand it.
    #[test]
    fn the_block_is_three_lines_and_keeps_the_arch_variable() {
        let config = repo_config("http://localhost:8080/api", None, None, None).unwrap();
        let lines: Vec<&str> = config.block.lines().collect();
        assert_eq!(lines.len(), 3, "{:?}", config.block);
        assert_eq!(lines[0], format!("[{DEFAULT_REPO_NAME}]"));
        assert!(lines[1].starts_with("SigLevel = "), "{:?}", lines[1]);
        assert!(lines[2].ends_with("/$arch"), "{:?}", lines[2]);
        assert!(config.block.ends_with('\n'), "must append cleanly");
    }

    #[test]
    fn the_name_port_and_siglevel_can_be_overridden() {
        let config = repo_config(
            "http://localhost:8080/api",
            Some(9000),
            Some("mine".to_string()),
            Some("Never".to_string()),
        )
        .unwrap();
        assert!(config.block.contains("[mine]"), "{:?}", config.block);
        assert!(
            config.block.contains("SigLevel = Never"),
            "{:?}",
            config.block
        );
        assert!(config.server.contains(":9000/"), "{:?}", config.server);
    }

    #[test]
    fn a_url_without_a_host_is_an_error() {
        assert!(repo_server_url("http://", 8081).is_err());
    }
}
