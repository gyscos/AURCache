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

use aurcache_common::worker::REPO_HOST_PLACEHOLDER;

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
///
/// `override_url` corresponds to `AURCACHE_REPO_URL` and replaces the base URL
/// outright — scheme, host, port and path. The host-only override assumes every
/// worker reaches the repository on the same scheme, port and path as every
/// other, which is the server's single `AURCACHE_PUBLIC_URL`; a worker behind a
/// reverse proxy (`https://aur.example.com/repo`) beside one on the compose
/// network (`http://aurcache:8081`) breaks that assumption. It wins over
/// `override_host`, being the same knob with a longer reach.
#[must_use]
pub fn render(
    template: &str,
    server_url: &str,
    override_host: Option<&str>,
    override_url: Option<&str>,
) -> String {
    if template.trim().is_empty() {
        return String::new();
    }
    if let Some(base) = override_url.map(str::trim).filter(|u| !u.is_empty()) {
        return with_base_url(template, base);
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

/// Point the template's `Server =` line at `base`, keeping the rest of it.
///
/// The rest is the server's to decide — `SigLevel`, and whatever else it may
/// put there — so only the one line the override is about is rewritten.
///
/// `/$arch` is appended because that is what the server does when it builds the
/// line from `AURCACHE_PUBLIC_URL`, and someone setting a base URL is answering
/// the same question. A `$arch` already present is left alone, so spelling it
/// out is not punished.
fn with_base_url(template: &str, base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let server = if trimmed.contains("$arch") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/$arch")
    };
    let mut out = String::new();
    for line in template.lines() {
        if line.trim_start().starts_with("Server =") {
            out.push_str(&format!("Server = {server}"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
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

    /// The host-only override cannot express a worker that reaches the
    /// repository on a different port or scheme from every other worker --
    /// which is what a reverse proxy in front of one of them means.
    #[test]
    fn a_base_url_override_replaces_more_than_the_host() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8081/$arch\n";

        let proxied = render(
            template,
            "https://aurcache:8083",
            None,
            Some("https://aur.example.com/repo"),
        );

        assert!(proxied.contains("Server = https://aur.example.com/repo/$arch"));
        // Everything the server decided that is not the URL survives.
        assert!(proxied.starts_with("[repo]\nSigLevel = Never\n"));
        assert!(!proxied.contains(REPO_HOST_PLACEHOLDER));
        assert!(!proxied.contains(":8081"));
    }

    /// The published port and the in-network port differ whenever compose maps
    /// one to the other. This is the case that sent a worker to a port nothing
    /// was listening on.
    #[test]
    fn a_base_url_override_can_change_only_the_port() {
        let template = "[repo]\nSigLevel = Never\nServer = http://%AURCACHE_HOST%:8091/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            None,
            Some("http://aurcache:8081"),
        );

        assert!(rendered.contains("Server = http://aurcache:8081/$arch"));
    }

    /// It is the same knob as the host override with a longer reach, so it
    /// wins rather than combining with it.
    #[test]
    fn the_base_url_override_beats_the_host_override() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            Some("repo.internal"),
            Some("http://elsewhere:9000"),
        );

        assert!(rendered.contains("Server = http://elsewhere:9000/$arch"));
        assert!(!rendered.contains("repo.internal"));
    }

    /// Spelling out `$arch` is what someone copying the existing line would
    /// do, and appending a second one would produce a URL that 404s.
    #[test]
    fn an_arch_already_in_the_override_is_left_alone() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(
            template,
            "https://aurcache:8083",
            None,
            Some("http://h/r/$arch"),
        );

        assert_eq!(rendered, "Server = http://h/r/$arch\n");
    }

    /// A blank value is an unset one, matching every other override here.
    #[test]
    fn a_blank_base_url_override_is_ignored() {
        let template = "Server = http://%AURCACHE_HOST%:8081/$arch\n";

        let rendered = render(template, "https://aurcache:8083", None, Some("   "));

        assert!(rendered.contains("Server = http://aurcache:8081/$arch"));
    }

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
        let compose = render(template, "https://aurcache:8083", None, None);
        let lan = render(template, "https://aur.example.com:8083", None, None);
        let embedded = render(template, "https://localhost:8083", None, None);

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
        let rendered = render(
            template,
            "https://aurcache:8083",
            Some("repo.internal"),
            None,
        );
        assert!(rendered.contains("http://repo.internal:8081"));
        // Blank is treated as unset rather than as an empty host.
        let blank = render(template, "https://aurcache:8083", Some("  "), None);
        assert!(blank.contains("http://aurcache:8081"));
    }

    /// Producing a section with an unresolved placeholder would hand pacman a
    /// URL it cannot use; producing none lets the build fail with a clear
    /// "unknown repo" instead.
    #[test]
    fn no_host_yields_no_section() {
        assert!(render("Server = %AURCACHE_HOST%", "://", None, None).is_empty());
    }

    /// A server that publishes no repository sends an empty template, and that
    /// must not become a section containing nothing useful.
    #[test]
    fn an_empty_template_renders_to_nothing() {
        assert!(render("", "https://aurcache:8083", None, None).is_empty());
    }

    #[test]
    fn appending_leaves_the_config_alone_when_there_is_no_section() {
        assert_eq!(append_to_pacman_conf("[options]\n", ""), "[options]\n");
        let with = append_to_pacman_conf("[options]", "[repo]\n");
        assert!(with.starts_with("[options]\n"));
        assert!(with.ends_with("[repo]\n"));
    }
}
