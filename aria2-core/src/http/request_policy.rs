//! Shared HTTP request policy for the Rust download paths.
//!
//! The public option names remain aria2-compatible, while this module keeps
//! their wire behavior in one internal seam.  It deliberately operates on a
//! reqwest request builder rather than exposing the downloader's internal
//! strategy to callers.

use reqwest::RequestBuilder;

/// Request-level HTTP behavior derived from [`DownloadOptions`].
///
/// The policy is cheap to clone and is passed to the sequential, segmented,
/// probe, and authentication retry paths so those paths cannot silently grow
/// different header behavior.
#[derive(Debug, Clone, Default)]
pub struct HttpRequestPolicy {
    headers: Vec<(String, String)>,
    pub accept_gzip: bool,
    pub no_cache: bool,
    pub want_digest: bool,
    pub keep_alive: bool,
    pub pipelining: bool,
}

impl HttpRequestPolicy {
    pub fn new(
        headers: Vec<(String, String)>,
        accept_gzip: bool,
        no_cache: bool,
        want_digest: bool,
        keep_alive: bool,
        pipelining: bool,
    ) -> Self {
        Self {
            headers,
            accept_gzip,
            no_cache,
            want_digest,
            keep_alive,
            pipelining,
        }
    }

    pub fn has_custom_headers(&self) -> bool {
        !self.headers.is_empty()
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case(name))
    }

    /// Apply custom and automatically generated headers to a request.
    ///
    /// `extra_headers` exists for compatibility with lower-level callers that
    /// historically supplied their own header slice. Duplicate names are
    /// suppressed case-insensitively so explicit user headers win over the
    /// generated defaults.
    pub fn apply(
        &self,
        mut request: RequestBuilder,
        cookie_header: Option<&str>,
        extra_headers: &[(String, String)],
    ) -> RequestBuilder {
        let mut names = Vec::with_capacity(self.headers.len() + extra_headers.len() + 8);
        for (name, value) in self.headers.iter().chain(extra_headers.iter()) {
            if names
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(name))
            {
                continue;
            }
            request = request.header(name, value);
            names.push(name.clone());
        }

        let has = |name: &str| names.iter().any(|known| known.eq_ignore_ascii_case(name));
        if let Some(cookie) = cookie_header
            && !cookie.is_empty()
            && !has("Cookie")
        {
            request = request.header("Cookie", cookie);
        }
        if self.accept_gzip && !has("Accept-Encoding") {
            request = request.header("Accept-Encoding", "deflate, gzip");
        }
        if self.no_cache {
            if !has("Pragma") {
                request = request.header("Pragma", "no-cache");
            }
            if !has("Cache-Control") {
                request = request.header("Cache-Control", "no-cache");
            }
        }
        if self.want_digest && !has("Want-Digest") {
            request = request.header("Want-Digest", "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1");
        }
        if !self.keep_alive && !self.pipelining && !has("Connection") {
            request = request.header("Connection", "close");
        }
        request
    }

    /// Apply the common policy and add generated Basic authorization unless a
    /// caller supplied an explicit Authorization header.
    pub fn apply_with_basic_auth(
        &self,
        request: RequestBuilder,
        cookie_header: Option<&str>,
        extra_headers: &[(String, String)],
        authorization: Option<&str>,
    ) -> RequestBuilder {
        let request = self.apply(request, cookie_header, extra_headers);
        let explicit_authorization = self.has_header("Authorization")
            || extra_headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"));
        if !explicit_authorization && let Some(authorization) = authorization {
            return request.header("Authorization", authorization);
        }
        request
    }
}

#[cfg(test)]
mod tests {
    use super::HttpRequestPolicy;

    #[test]
    fn applies_aria2_http_headers_and_preserves_explicit_values() {
        crate::http::client_pool::ensure_rustls_provider();
        let policy = HttpRequestPolicy::new(
            vec![
                ("X-Test".into(), "custom".into()),
                ("Pragma".into(), "keep".into()),
            ],
            true,
            true,
            true,
            false,
            false,
        );
        let request = policy
            .apply(
                reqwest::Client::new().get("http://example.test/file"),
                Some("sid=abc"),
                &[("X-Test".into(), "duplicate".into())],
            )
            .build()
            .expect("request builds");

        assert_eq!(request.headers().get("x-test").unwrap(), "custom");
        assert_eq!(request.headers().get("pragma").unwrap(), "keep");
        assert_eq!(request.headers().get("cache-control").unwrap(), "no-cache");
        assert_eq!(
            request.headers().get("accept-encoding").unwrap(),
            "deflate, gzip"
        );
        assert_eq!(
            request.headers().get("want-digest").unwrap(),
            "SHA-512;q=1, SHA-256;q=1, SHA;q=0.1"
        );
        assert_eq!(request.headers().get("connection").unwrap(), "close");
        assert_eq!(request.headers().get("cookie").unwrap(), "sid=abc");
    }

    #[test]
    fn disabled_digest_and_keep_alive_do_not_emit_headers() {
        crate::http::client_pool::ensure_rustls_provider();
        let policy = HttpRequestPolicy::new(Vec::new(), false, false, false, true, false);
        let request = policy
            .apply(
                reqwest::Client::new().get("http://example.test/file"),
                None,
                &[],
            )
            .build()
            .expect("request builds");
        assert!(request.headers().get("want-digest").is_none());
        assert!(request.headers().get("connection").is_none());
        assert!(request.headers().get("accept-encoding").is_none());
    }

    #[test]
    fn generated_basic_auth_does_not_override_explicit_authorization() {
        crate::http::client_pool::ensure_rustls_provider();
        let policy = HttpRequestPolicy::default();
        let request = policy
            .apply_with_basic_auth(
                reqwest::Client::new().get("http://example.test/file"),
                None,
                &[],
                Some("Basic generated"),
            )
            .build()
            .expect("request builds");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Basic generated"
        );

        let request = policy
            .apply_with_basic_auth(
                reqwest::Client::new().get("http://example.test/file"),
                None,
                &[(
                    String::from("Authorization"),
                    String::from("Bearer explicit"),
                )],
                Some("Basic generated"),
            )
            .build()
            .expect("request builds");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer explicit"
        );
    }
}
