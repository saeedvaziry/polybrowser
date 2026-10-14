//! HTTP cookies, as read from and written to an engine's cookie store or an
//! installed browser.

use serde::{Deserialize, Serialize};

use crate::Timestamp;

/// One HTTP cookie.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// The host the cookie belongs to. With a leading dot (`.example.com`) it
    /// is also sent to subdomains; without one it is host-only.
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// When the cookie expires. `None` is a session cookie.
    pub expires: Option<Timestamp>,
}

impl Cookie {
    /// Whether this cookie would be sent to `url`, by the usual rules: the
    /// host must match the cookie's domain (a leading dot or a host-only
    /// cookie both allow the exact host, a dotted domain also allows
    /// subdomains), the request path must be under the cookie's path, and a
    /// `secure` cookie only travels over `https`.
    pub fn matches_url(&self, url: &str) -> bool {
        let Some((scheme, rest)) = url.split_once("://") else {
            return false;
        };
        let scheme = scheme.to_ascii_lowercase();
        if self.secure && scheme != "https" {
            return false;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or(authority);
        // Strip a port, keeping IPv6 literals intact.
        let host = match host.strip_prefix('[') {
            Some(inner) => inner.split(']').next().unwrap_or(inner),
            None => host.split(':').next().unwrap_or(host),
        };
        let path = {
            let after_authority = &rest[authority.len()..];
            let path = after_authority.split(['?', '#']).next().unwrap_or("");
            if path.is_empty() {
                "/"
            } else {
                path
            }
        };
        self.matches_host(host) && path_matches(&self.path, path)
    }

    fn matches_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let domain = self
            .domain
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if domain.is_empty() {
            return false;
        }
        host == domain || (self.domain.starts_with('.') && host.ends_with(&format!(".{domain}")))
    }
}

/// Whether `request_path` is `cookie_path` or below it, by RFC 6265 path
/// matching.
fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    let cookie_path = if cookie_path.is_empty() {
        "/"
    } else {
        cookie_path
    };
    if request_path == cookie_path {
        return true;
    }
    if let Some(rest) = request_path.strip_prefix(cookie_path) {
        return cookie_path.ends_with('/') || rest.starts_with('/');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(domain: &str, path: &str, secure: bool) -> Cookie {
        Cookie {
            name: "k".into(),
            value: "v".into(),
            domain: domain.into(),
            path: path.into(),
            secure,
            http_only: false,
            expires: None,
        }
    }

    #[test]
    fn domain_and_path_matching() {
        let c = cookie(".example.com", "/app", false);
        assert!(c.matches_url("http://example.com/app"));
        assert!(c.matches_url("http://www.example.com/app/page"));
        assert!(!c.matches_url("http://example.com/other"));
        assert!(!c.matches_url("http://notexample.com/app"));

        let host_only = cookie("example.com", "/", false);
        assert!(host_only.matches_url("https://example.com/"));
        assert!(!host_only.matches_url("https://sub.example.com/"));
        assert!(!host_only.matches_url("https://evilexample.com/"));
    }

    #[test]
    fn secure_cookies_need_https() {
        let c = cookie("example.com", "/", true);
        assert!(c.matches_url("https://example.com/"));
        assert!(!c.matches_url("http://example.com/"));
    }

    #[test]
    fn ports_and_userinfo_are_ignored() {
        let c = cookie("example.com", "/", false);
        assert!(c.matches_url("http://user@example.com:8080/x"));
    }
}
