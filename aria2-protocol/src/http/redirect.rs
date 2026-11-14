//! Origin scope and credential-header policy for HTTP redirects.

/// Compare HTTP origins using scheme, host, and effective port.
pub fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.origin() == right.origin()
}

/// Identify standard and conventional application credentials in custom
/// headers so they are dropped together on a cross-origin redirect.
pub fn is_sensitive_header(name: &str) -> bool {
    if ["authorization", "proxy-authorization", "cookie", "cookie2"]
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
    {
        return true;
    }

    name.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| {
            [
                "auth",
                "authentication",
                "bearer",
                "credential",
                "credentials",
                "csrf",
                "key",
                "password",
                "passwd",
                "secret",
                "session",
                "signature",
                "token",
                "xsrf",
            ]
            .iter()
            .any(|sensitive| part.eq_ignore_ascii_case(sensitive))
        })
}

/// Tracks whether a redirect chain has crossed an origin boundary. Once
/// sensitive headers are removed, later redirects cannot restore them.
#[derive(Debug, Clone)]
pub struct RedirectOriginScope {
    initial_url: url::Url,
    crossed_origin: bool,
}

impl RedirectOriginScope {
    pub fn new(initial_url: url::Url) -> Self {
        Self {
            initial_url,
            crossed_origin: false,
        }
    }

    pub fn with_sensitive_headers_allowed(mut self, allowed: bool) -> Self {
        self.crossed_origin = !allowed;
        self
    }

    pub fn allows_sensitive_headers(&self, target: &url::Url) -> bool {
        !self.crossed_origin && same_origin(&self.initial_url, target)
    }

    pub fn follow_redirect(&mut self, current: &url::Url, target: &url::Url) {
        self.crossed_origin |= !same_origin(current, target);
    }

    pub fn remove_sensitive_headers(&mut self) {
        self.crossed_origin = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{RedirectOriginScope, is_sensitive_header, same_origin};

    #[test]
    fn origin_comparison_includes_scheme_host_and_effective_port() {
        let initial = url::Url::parse("https://example.test/file").unwrap();
        assert!(same_origin(
            &initial,
            &url::Url::parse("https://example.test:443/other").unwrap()
        ));
        assert!(!same_origin(
            &initial,
            &url::Url::parse("http://example.test/file").unwrap()
        ));
        assert!(!same_origin(
            &initial,
            &url::Url::parse("https://cdn.example.test/file").unwrap()
        ));
        assert!(!same_origin(
            &initial,
            &url::Url::parse("https://example.test:8443/file").unwrap()
        ));
    }

    #[test]
    fn crossing_an_origin_boundary_strips_credentials_for_the_rest_of_the_chain() {
        let initial = url::Url::parse("https://example.test/file").unwrap();
        let other = url::Url::parse("https://cdn.example.test/file").unwrap();
        let mut scope = RedirectOriginScope::new(initial.clone());

        assert!(scope.allows_sensitive_headers(&initial));
        scope.follow_redirect(&initial, &other);
        assert!(!scope.allows_sensitive_headers(&other));
        scope.follow_redirect(&other, &initial);
        assert!(!scope.allows_sensitive_headers(&initial));
    }

    #[test]
    fn recognizes_standard_and_conventional_sensitive_headers() {
        for name in [
            "Authorization",
            "Cookie",
            "X-API-Key",
            "X-Session-Token",
            "X-CSRF-Token",
        ] {
            assert!(is_sensitive_header(name), "{name}");
        }
        assert!(!is_sensitive_header("X-Trace-ID"));
    }
}
