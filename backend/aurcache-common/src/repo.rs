//! How the pacman repository is addressed.
//!
//! The server knows how the repository is *published* — scheme, port, path,
//! whatever `AURCACHE_PUBLIC_URL` says, possibly through a reverse proxy. It
//! does not reliably know the *host* each client reaches it by: a worker in the
//! compose network, one on the LAN and a laptop running `pacman -Sy` all get
//! there by different names.
//!
//! So the split is always the same, and it is worth having in one place: take
//! everything but the host from the server, and the host from whoever is asking.

use crate::ports::AURCACHE_MIRROR_PORT;

/// The `Server =` line for a repository published at `public_url`, as reached by
/// a client that gets to the instance at `host`.
///
/// `$arch` is appended for pacman to expand, so one line serves every
/// architecture the instance builds.
#[must_use]
pub fn server_url_for_host(public_url: &str, host: &str) -> String {
    let trimmed = public_url.trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .map_or(("http", trimmed), |(scheme, rest)| (scheme, rest));
    let (authority, path) = rest
        .find('/')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));

    // A URL without an explicit port still needs one, or the result points at
    // the web UI instead of the repository. A bracketed `[v6]` authority has
    // colons of its own, so the port is read past the `]`, not after the
    // last `:` (which would read `:1]` out of `[::1]` as a port).
    let port = if let Some(inside) = authority.strip_prefix('[') {
        inside.split_once("]:").map_or_else(
            || format!(":{AURCACHE_MIRROR_PORT}"),
            |(_, p)| format!(":{p}"),
        )
    } else {
        authority.rsplit_once(':').map_or_else(
            || format!(":{AURCACHE_MIRROR_PORT}"),
            |(_, p)| format!(":{p}"),
        )
    };

    // Brackets for a bare IPv6 literal: without them the port merges into
    // the address and pacman reads garbage.
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };

    format!("{scheme}://{host}{port}{path}/$arch")
}

#[cfg(test)]
mod tests {
    use super::server_url_for_host;
    use crate::ports::AURCACHE_MIRROR_PORT;

    /// Only the host changes: the rest describes how the repository is
    /// published and stays the server's decision.
    #[test]
    fn replaces_only_the_host() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com:9000/repo", "HOST"),
            "http://HOST:9000/repo/$arch"
        );
    }

    #[test]
    fn supplies_the_repository_port_when_the_url_omits_it() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com", "HOST"),
            format!("http://HOST:{AURCACHE_MIRROR_PORT}/$arch")
        );
    }

    #[test]
    fn keeps_https_and_ignores_a_trailing_slash() {
        assert_eq!(
            server_url_for_host("https://aurcache.example.com:8081/", "HOST"),
            "https://HOST:8081/$arch"
        );
    }

    /// A URL someone typed without a scheme still has to produce a usable line.
    #[test]
    fn a_missing_scheme_reads_as_http() {
        assert_eq!(
            server_url_for_host("aurcache.example.com:8081", "HOST"),
            "http://HOST:8081/$arch"
        );
    }

    /// A port is supplied when the URL omits one, because `http://host/$arch`
    /// would otherwise reach the web UI rather than the repository.
    ///
    /// Inherited from the worker template this was extracted from, and wrong
    /// for one case: a reverse proxy publishing on 443 under a path gets a
    /// spurious `:8081`. Setting `AURCACHE_PUBLIC_URL` with an explicit port
    /// (`https://host:443/arch`) is the workaround. Changing it would change
    /// what every worker is told, so it is deliberately not changed here.
    #[test]
    fn a_path_is_preserved_and_a_missing_port_is_supplied() {
        assert_eq!(
            server_url_for_host("https://aurcache.example.com/arch", "HOST"),
            format!("https://HOST:{AURCACHE_MIRROR_PORT}/arch/$arch")
        );
    }

    /// An IPv6 host needs brackets, or the port merges into the address and
    /// pacman reads garbage.
    #[test]
    fn an_ipv6_host_is_bracketed() {
        assert_eq!(
            server_url_for_host("http://aurcache.example.com:9000/repo", "::1"),
            "http://[::1]:9000/repo/$arch"
        );
    }

    /// ...and an IPv6 public URL keeps its own port rather than reading
    /// `:1]` out of `[::1]` as one.
    #[test]
    fn an_ipv6_public_url_keeps_its_port() {
        assert_eq!(
            server_url_for_host("http://[fd00::1]:9000/repo", "worker"),
            "http://worker:9000/repo/$arch"
        );
        assert_eq!(
            server_url_for_host("http://[fd00::1]/repo", "worker"),
            format!("http://worker:{AURCACHE_MIRROR_PORT}/repo/$arch")
        );
    }
}
