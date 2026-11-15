use crate::http::socks_connector::NoProxyMatcher;
use aria2_protocol::proxy::ProxyUrl;

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
        if self.no_proxy.as_deref().is_some_and(|no_proxy| {
            NoProxyMatcher::from_env_value(no_proxy).should_bypass_hostname(host)
        }) {
            return None;
        }
        let proxy = match parsed_uri.scheme() {
            "http" => self
                .http_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
                .or_else(|| self.all_proxy.as_deref().filter(|proxy| !proxy.is_empty())),
            "https" => self
                .https_proxy
                .as_deref()
                .filter(|proxy| !proxy.is_empty())
                .or_else(|| self.all_proxy.as_deref().filter(|proxy| !proxy.is_empty())),
            _ => None,
        }?;
        let proxy = ProxyUrl::parse(proxy).ok()?;
        Some((proxy.host, proxy.port))
    }

    pub(crate) fn credentials_for_scheme(&self, scheme: &str) -> (Option<String>, Option<String>) {
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
            .and_then(|value| ProxyUrl::parse(value).ok())
            .map(|proxy| (proxy.username, proxy.password))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_for_scheme_decodes_embedded_proxy_userinfo() {
        let options = super::super::DownloadOptions {
            http_proxy: Some("http://user%40name:p%3A%40%25@proxy.example:8080".to_string()),
            ..Default::default()
        };
        let proxy_options = HttpProxyOptions::from_download_options(&options);

        assert_eq!(
            proxy_options.credentials_for_scheme("http"),
            (Some("user@name".to_string()), Some("p:@%".to_string()))
        );
    }
}
