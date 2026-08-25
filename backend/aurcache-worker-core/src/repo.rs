//! Rendering the `[repo]` section a build uses to resolve AURCache packages.
//!
//! The server sends a template at registration with the host left as
//! [`REPO_HOST_PLACEHOLDER`], because the host is the one part it cannot know:
//! workers reach the same server by different addresses — a compose service
//! name, a LAN address, `localhost` for a worker embedded beside it — and a
//! single baked-in value is wrong for all but one of them. Everything else
//! about the section stays the server's choice.
//!
//! The worker already knows the right answer: it is the host it dialled to
//! reach the server in the first place.

use aurcache_types::worker::REPO_HOST_PLACEHOLDER;

/// Extract the host from a base URL such as `https://aurcache:8083`.
///
/// Returns `None` when no host can be determined, in which case no `[repo]`
/// section is produced at all — better than emitting one pointing at a guess.
#[must_use]
pub fn host_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip any userinfo, then any port. IPv6 literals keep their brackets.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let host = match authority.rfind(']') {
        Some(end) => &authority[..=end],
        None => authority.split(':').next().unwrap_or(""),
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Render the server's template for this worker.
///
/// An empty result means no `[repo]` section: either the server publishes no
/// repository, or no host could be determined. Emitting a section containing an
/// unresolved placeholder would be worse — pacman would fail on a URL nobody
/// wrote, instead of simply not knowing the repository.
///
/// `override_host` corresponds to `AURCACHE_REPO_HOST`, for deployments where
/// the address a worker uses for the protocol is not the address it should use
/// for the repository.
#[must_use]
pub fn render(template: &str, server_url: &str, override_host: Option<&str>) -> String {
    if template.trim().is_empty() {
        return String::new();
    }
    let host = match override_host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => host.to_string(),
        None => match host_from_url(server_url) {
            Some(host) => host,
            None => return String::new(),
        },
    };
    template.replace(REPO_HOST_PLACEHOLDER, &host)
}

/// Append a rendered `[repo]` section to a job's `pacman.conf`.
///
/// An empty section is appended as nothing.
#[must_use]
pub fn append_to_pacman_conf(pacman_conf: &str, section: &str) -> String {
    if section.trim().is_empty() {
        return pacman_conf.to_string();
    }
    let mut out = pacman_conf.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(section);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_host_from_the_server_url() {
        assert_eq!(
            host_from_url("https://aurcache:8083").as_deref(),
            Some("aurcache")
        );
        assert_eq!(
            host_from_url("https://localhost:8083").as_deref(),
            Some("localhost")
        );
        assert_eq!(
            host_from_url("https://aur.example.com").as_deref(),
            Some("aur.example.com")
        );
        assert_eq!(
            host_from_url("https://10.0.0.5:8083/api").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(host_from_url("aurcache:8083").as_deref(), Some("aurcache"));
    }

    /// An IPv6 literal keeps its brackets; splitting on `:` would mangle it.
    #[test]
    fn keeps_ipv6_literals_intact() {
        assert_eq!(
            host_from_url("https://[fd00::1]:8083").as_deref(),
            Some("[fd00::1]")
        );
    }

    /// Each worker must get the host *it* uses, which is the entire point of
    /// templating rather than baking the URL in server-side.
    #[test]
    fn renders_a_different_host_per_worker() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8081/$arch\n";
        let compose = render(template, "https://aurcache:8083", None);
        let lan = render(template, "https://aur.example.com:8083", None);
        let embedded = render(template, "https://localhost:8083", None);

        assert!(compose.contains("Server = http://aurcache:8081/$arch"));
        assert!(lan.contains("Server = http://aur.example.com:8081/$arch"));
        assert!(embedded.contains("Server = http://localhost:8081/$arch"));
        // The port and section name stay exactly as the server chose them.
        for rendered in [&compose, &lan, &embedded] {
            assert!(rendered.starts_with("[repo]\nSigLevel = Never\n"));
            assert!(!rendered.contains(REPO_HOST_PLACEHOLDER));
        }
    }

    #[test]
    fn an_explicit_override_wins() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";
        let rendered = render(template, "https://aurcache:8083", Some("repo.internal"));
        assert!(rendered.contains("http://repo.internal:8081"));
        // Blank is treated as unset rather than as an empty host.
        let blank = render(template, "https://aurcache:8083", Some("  "));
        assert!(blank.contains("http://aurcache:8081"));
    }

    /// Producing a section with an unresolved placeholder would hand pacman a
    /// URL it cannot use; producing none lets the build fail with a clear
    /// "unknown repo" instead.
    #[test]
    fn no_host_yields_no_section() {
        assert!(render("Server = %AURCACHE_HOST%", "://", None).is_empty());
    }

    /// A server that publishes no repository sends an empty template, and that
    /// must not become a section containing nothing useful.
    #[test]
    fn an_empty_template_renders_to_nothing() {
        assert!(render("", "https://aurcache:8083", None).is_empty());
    }

    #[test]
    fn appending_leaves_the_config_alone_when_there_is_no_section() {
        assert_eq!(append_to_pacman_conf("[options]\n", ""), "[options]\n");
        let with = append_to_pacman_conf("[options]", "[repo]\n");
        assert!(with.starts_with("[options]\n"));
        assert!(with.ends_with("[repo]\n"));
    }
}
