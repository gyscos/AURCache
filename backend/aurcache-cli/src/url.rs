//! Host arithmetic on the configured API URL.
//!
//! The CLI is configured with the *API* URL (`http://host:8080/api`), but the
//! questions a new user has next are about other things the same machine
//! serves: the pacman repository on its own port, and the worker protocol on
//! another. Those are all "same host, different port", so the one operation
//! worth sharing is pulling the host back out of a URL.

/// The host in `url`, without scheme, userinfo, port or path.
///
/// An IPv6 literal keeps its brackets: that is how it has to be written back
/// into a URL, and dropping them would produce an address that does not parse.
#[must_use]
pub fn host_from_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo is delimited by the *last* `@`, since a password may contain one.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    let host = if authority.starts_with('[') {
        &authority[..=authority.find(']')?]
    } else {
        authority.split(':').next()?
    };

    (!host.is_empty()).then_some(host)
}

/// The scheme in `url`, defaulting to `http` when it carries none.
///
/// A scheme-less URL is what someone types when they mean "the obvious thing",
/// and every port this CLI derives is plain HTTP unless the API itself is TLS.
#[must_use]
pub fn scheme_from_url(url: &str) -> &str {
    url.split_once("://").map_or("http", |(scheme, _)| scheme)
}

#[cfg(test)]
mod tests {
    use super::{host_from_url, scheme_from_url};

    #[test]
    fn a_host_is_read_out_of_the_usual_shapes() {
        assert_eq!(
            host_from_url("http://localhost:8080/api"),
            Some("localhost")
        );
        assert_eq!(
            host_from_url("https://aurcache.example.com"),
            Some("aurcache.example.com")
        );
        assert_eq!(
            host_from_url("http://192.168.1.10:8080/"),
            Some("192.168.1.10")
        );
        // No scheme at all: the whole thing is the authority.
        assert_eq!(host_from_url("localhost:8080/api"), Some("localhost"));
    }

    /// The brackets are part of the address once it goes back into a URL.
    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        assert_eq!(host_from_url("http://[::1]:8080/api"), Some("[::1]"));
        assert_eq!(
            host_from_url("http://[2001:db8::1]/api"),
            Some("[2001:db8::1]")
        );
    }

    /// A password may itself contain `@`, so the split has to be from the right.
    #[test]
    fn userinfo_is_stripped_from_the_last_separator() {
        assert_eq!(
            host_from_url("http://user:p@ss@host:8080/api"),
            Some("host")
        );
    }

    #[test]
    fn a_url_without_a_host_has_none() {
        assert_eq!(host_from_url(""), None);
        assert_eq!(host_from_url("http://"), None);
        assert_eq!(host_from_url("http:///api"), None);
    }

    #[test]
    fn a_missing_scheme_reads_as_http() {
        assert_eq!(scheme_from_url("https://host/api"), "https");
        assert_eq!(scheme_from_url("http://host/api"), "http");
        assert_eq!(scheme_from_url("host:8080/api"), "http");
    }
}
