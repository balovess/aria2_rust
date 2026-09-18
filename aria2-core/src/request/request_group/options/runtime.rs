use super::{DownloadOptions, FollowMode};
use url::Url;

impl DownloadOptions {
    /// Whether a memory follow mode is configured.
    ///
    /// This is an option-level predicate only. Callers that are deciding how
    /// to download a source must also check its URI or response content type
    /// with [`Self::uses_memory_download_for_uri`] or
    /// [`Self::uses_memory_download_for_content_type`].
    pub fn uses_memory_download(&self) -> bool {
        self.follow_torrent.is_some_and(FollowMode::is_memory)
            || self.follow_metalink.is_some_and(FollowMode::is_memory)
    }

    /// Whether a source URI should use memory-backed metadata handling.
    ///
    /// The `follow-*=mem` options apply to recognized metadata sources, not
    /// to every HTTP/FTP/SFTP payload in the request group. URI matching
    /// mirrors aria2's suffix criteria and ignores URL query/fragment parts.
    pub fn uses_memory_download_for_uri(&self, uri: &str) -> bool {
        let path = Url::parse(uri)
            .map(|url| url.path().to_owned())
            .unwrap_or_else(|_| uri.to_owned());
        let path = path.to_ascii_lowercase();

        (self.follow_torrent.is_some_and(FollowMode::is_memory) && path.ends_with(".torrent"))
            || (self.follow_metalink.is_some_and(FollowMode::is_memory)
                && [".meta4", ".metalink", ".metalink3"]
                    .iter()
                    .any(|extension| path.ends_with(extension)))
    }

    /// Whether a response content type identifies a memory-backed metadata
    /// source for the configured follow mode.
    pub fn uses_memory_download_for_content_type(&self, content_type: &str) -> bool {
        let content_type = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();

        (self.follow_torrent.is_some_and(FollowMode::is_memory)
            && content_type == "application/x-bittorrent")
            || (self.follow_metalink.is_some_and(FollowMode::is_memory)
                && matches!(
                    content_type.as_str(),
                    "application/metalink4+xml"
                        | "application/metalink+xml"
                        | "application/x-metalink"
                ))
    }

    /// Return the configured write-back capacity, or `None` when disabled.
    pub fn disk_cache_size_bytes(&self) -> Option<u64> {
        self.disk_cache.filter(|size| *size > 0)
    }

    /// Parse the raw `header` list into `(name, value)` pairs, splitting each
    /// `"Name: Value"` entry on the first `:`. When `user_agent` or `referer`
    /// are set, they are appended as `User-Agent` / `Referer` pairs (unless an
    /// entry with the same name already exists), so callers only need to handle
    /// a single header list.
    pub fn parsed_headers(&self) -> Vec<(String, String)> {
        let mut result: Vec<(String, String)> = Vec::new();
        for raw in &self.header {
            if let Some((name, value)) = raw.split_once(':') {
                let name = name.trim().to_string();
                let value = value.trim().to_string();
                if !name.is_empty() {
                    result.push((name, value));
                }
            }
        }
        // Overlay user_agent / referer if not already present (case-insensitive).
        if let Some(ref ua) = self.user_agent
            && !has_header(&result, "User-Agent")
        {
            result.push(("User-Agent".to_string(), ua.clone()));
        }
        if let Some(ref ref_) = self.referer
            && !has_header(&result, "Referer")
        {
            result.push(("Referer".to_string(), ref_.clone()));
        }
        result
    }

    /// Build the internal HTTP request policy shared by every HTTP request
    /// path. The option names and defaults remain exposed through the aria2
    /// compatible configuration/RPC surfaces.
    pub fn http_request_policy(&self) -> crate::http::HttpRequestPolicy {
        crate::http::HttpRequestPolicy::new(
            self.parsed_headers(),
            self.http_accept_gzip,
            self.http_no_cache,
            !self.no_want_digest_header,
            self.enable_http_keep_alive,
            self.enable_http_pipelining,
        )
        .with_browser_context(crate::http::global_browser_context())
    }

    /// Resolve proxy credentials using aria2's protocol-specific precedence.
    ///
    /// A protocol-specific credential overrides the corresponding
    /// `all-proxy-*` value. The `all` selector is used when constructing the
    /// fallback proxy matcher itself.
    pub(crate) fn proxy_credentials_for_scheme(
        &self,
        scheme: &str,
    ) -> (Option<String>, Option<String>) {
        let (user, passwd, proxy_url) = match scheme {
            "https" => (
                &self.https_proxy_user,
                &self.https_proxy_passwd,
                self.https_proxy
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .or_else(|| self.all_proxy.as_deref().filter(|value| !value.is_empty())),
            ),
            "http" => (
                &self.http_proxy_user,
                &self.http_proxy_passwd,
                self.http_proxy
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .or_else(|| self.all_proxy.as_deref().filter(|value| !value.is_empty())),
            ),
            "ftp" => (
                &self.ftp_proxy_user,
                &self.ftp_proxy_passwd,
                self.ftp_proxy
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .or_else(|| self.all_proxy.as_deref().filter(|value| !value.is_empty())),
            ),
            _ => (
                &self.all_proxy_user,
                &self.all_proxy_passwd,
                self.all_proxy.as_deref().filter(|value| !value.is_empty()),
            ),
        };

        let embedded = proxy_url
            .and_then(|value| Url::parse(value).ok())
            .map(|url| {
                (
                    (!url.username().is_empty()).then(|| url.username().to_string()),
                    url.password().map(str::to_string),
                )
            })
            .unwrap_or((None, None));

        (
            user.clone()
                .or_else(|| self.all_proxy_user.clone())
                .or(embedded.0),
            passwd
                .clone()
                .or_else(|| self.all_proxy_passwd.clone())
                .or(embedded.1),
        )
    }
}

/// Case-insensitive check whether a `(name, value)` header list already contains
/// an entry with the given name.
fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}
