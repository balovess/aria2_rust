use crate::http::socks_connector::NoProxyMatcher;
use aria2_protocol::proxy::ProxyUrl;

struct SelectedProxy<'a> {
    raw: &'a str,
    parsed: ProxyUrl,
    username: Option<&'a str>,
    password: Option<&'a str>,
}

/// Per-task HTTP proxy selection and authentication settings.
#[derive(Clone, Debug, Default)]
pub(crate) struct HttpProxyOptions {
    pub(crate) http_proxy: Option<String>,
    pub(crate) https_proxy: Option<String>,
    pub(crate) all_proxy: Option<String>,
    pub(crate) no_proxy: Option<String>,
    ftp_proxy: Option<String>,
    http_proxy_user: Option<String>,
    http_proxy_passwd: Option<String>,
    https_proxy_user: Option<String>,
    https_proxy_passwd: Option<String>,
    ftp_proxy_user: Option<String>,
    ftp_proxy_passwd: Option<String>,
    all_proxy_user: Option<String>,
    all_proxy_passwd: Option<String>,
}

impl HttpProxyOptions {
    pub(crate) fn from_download_options(options: &super::DownloadOptions) -> Self {
        Self {
            http_proxy: options.http_proxy.clone(),
            https_proxy: options.https_proxy.clone(),
            all_proxy: options.all_proxy.clone(),
            no_proxy: options.no_proxy.clone(),
            ftp_proxy: options.ftp_proxy.clone(),
            http_proxy_user: options.http_proxy_user.clone(),
            http_proxy_passwd: options.http_proxy_passwd.clone(),
            https_proxy_user: options.https_proxy_user.clone(),
            https_proxy_passwd: options.https_proxy_passwd.clone(),
            ftp_proxy_user: options.ftp_proxy_user.clone(),
            ftp_proxy_passwd: options.ftp_proxy_passwd.clone(),
            all_proxy_user: options.all_proxy_user.clone(),
            all_proxy_passwd: options.all_proxy_passwd.clone(),
        }
    }

    pub(crate) fn proxy_for_uri(&self, uri: &str) -> Option<(String, u16)> {
        let parsed_uri = url::Url::parse(uri).ok()?;
        let host = parsed_uri.host_str()?;
        if self.bypasses_no_proxy(host) {
            return None;
        }
        let proxy = self.selected_proxy(parsed_uri.scheme())?;
        Some((proxy.parsed.host, proxy.parsed.port))
    }

    /// Return the configured proxy as a URL suitable for external HTTP
    /// clients, including credentials and protocol-specific selection.
    pub(crate) fn proxy_url_for_uri(&self, uri: &str) -> Option<String> {
        let parsed_uri = url::Url::parse(uri).ok()?;
        let host = parsed_uri.host_str()?;
        if self.bypasses_no_proxy(host) {
            return None;
        }
        let selected = self.selected_proxy(parsed_uri.scheme())?;
        let mut proxy_url = url::Url::parse(selected.raw).ok()?;
        if let Some(username) = selected.username.or(selected.parsed.username.as_deref()) {
            proxy_url.set_username(username).ok()?;
        }
        if let Some(password) = selected.password.or(selected.parsed.password.as_deref()) {
            proxy_url.set_password(Some(password)).ok()?;
        }
        Some(proxy_url.to_string())
    }

    pub(crate) fn credentials_for_scheme(&self, scheme: &str) -> (Option<String>, Option<String>) {
        let Some(proxy) = self.selected_proxy(scheme) else {
            return (None, None);
        };
        (
            proxy.username.map(str::to_owned).or(proxy.parsed.username),
            proxy.password.map(str::to_owned).or(proxy.parsed.password),
        )
    }

    fn bypasses_no_proxy(&self, host: &str) -> bool {
        self.no_proxy.as_deref().is_some_and(|no_proxy| {
            NoProxyMatcher::from_env_value(no_proxy).should_bypass_hostname(host)
        })
    }

    fn selected_proxy(&self, scheme: &str) -> Option<SelectedProxy<'_>> {
        let specific = match scheme {
            "http" => Some((
                self.http_proxy.as_deref(),
                self.http_proxy_user.as_deref(),
                self.http_proxy_passwd.as_deref(),
            )),
            "https" => Some((
                self.https_proxy.as_deref(),
                self.https_proxy_user.as_deref(),
                self.https_proxy_passwd.as_deref(),
            )),
            "ftp" => Some((
                self.ftp_proxy.as_deref(),
                self.ftp_proxy_user.as_deref(),
                self.ftp_proxy_passwd.as_deref(),
            )),
            _ => None,
        };

        if let Some((Some(proxy_url), username, password)) = specific
            && !proxy_url.is_empty()
            && let Ok(parsed) = ProxyUrl::parse(proxy_url)
        {
            return Some(SelectedProxy {
                raw: proxy_url,
                parsed,
                username,
                password,
            });
        }

        let proxy_url = self
            .all_proxy
            .as_deref()
            .filter(|value| !value.is_empty())?;
        let parsed = ProxyUrl::parse(proxy_url).ok()?;
        Some(SelectedProxy {
            raw: proxy_url,
            parsed,
            username: self.all_proxy_user.as_deref(),
            password: self.all_proxy_passwd.as_deref(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_for_scheme_uses_the_selected_proxy_scope() {
        let options = super::super::DownloadOptions {
            http_proxy: Some("http://url-user:url-pass@specific-proxy.example:8080".to_string()),
            all_proxy: Some("http://all-url-user:all-url-pass@fallback-proxy.example:8080".into()),
            http_proxy_user: Some("http-user".into()),
            all_proxy_user: Some("all-user".into()),
            all_proxy_passwd: Some("all-pass".into()),
            ..Default::default()
        };
        let proxy_options = HttpProxyOptions::from_download_options(&options);

        assert_eq!(
            proxy_options.credentials_for_scheme("http"),
            (Some("http-user".to_string()), Some("url-pass".to_string()))
        );
        assert_eq!(
            proxy_options.credentials_for_scheme("https"),
            (Some("all-user".to_string()), Some("all-pass".to_string()))
        );
    }

    #[test]
    fn invalid_specific_proxy_falls_back_with_all_proxy_credentials() {
        let options = super::super::DownloadOptions {
            http_proxy: Some("http://proxy.invalid:bad".into()),
            all_proxy: Some("http://all-user:all-pass@fallback-proxy.example:8080".into()),
            http_proxy_user: Some("must-not-leak".into()),
            ..Default::default()
        };
        let proxy_options = HttpProxyOptions::from_download_options(&options);

        assert_eq!(
            proxy_options.proxy_for_uri("http://target.example/file"),
            Some(("fallback-proxy.example".into(), 8080))
        );
        assert_eq!(
            proxy_options.credentials_for_scheme("http"),
            (Some("all-user".into()), Some("all-pass".into()))
        );
    }
}
