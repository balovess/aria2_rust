//! Shared HTTP authentication context construction.
//!
//! The request and response adapters both need the same credential precedence:
//! URL credentials, activated challenge credentials, explicit options, and
//! Netrc. Keeping that construction behind one small interface prevents the
//! metadata preflight and the streaming downloader from drifting apart.

use crate::http::{AuthConfigFactory, AuthResolveOptions};
use crate::request::request_group::DownloadOptions;

pub(crate) fn factory_from_netrc(netrc_path: Option<&str>, no_netrc: bool) -> AuthConfigFactory {
    let mut factory = AuthConfigFactory::new();
    if !no_netrc
        && let Some(path) = netrc_path
        && let Err(error) = factory.load_netrc_file(std::path::Path::new(path))
    {
        tracing::debug!(path, %error, "Failed to load HTTP authentication netrc");
    }
    factory
}

pub(crate) struct HttpAuthContext {
    pub(crate) factory: AuthConfigFactory,
    pub(crate) options: AuthResolveOptions,
}

pub(crate) fn from_options(options: &DownloadOptions, scheme: &str) -> HttpAuthContext {
    HttpAuthContext {
        factory: factory_from_options(options),
        options: auth_options_from_download_options(options, scheme),
    }
}

pub(crate) fn auth_options_from_download_options(
    options: &DownloadOptions,
    scheme: &str,
) -> AuthResolveOptions {
    let (proxy_user, proxy_passwd) = options.proxy_credentials_for_scheme(scheme);
    AuthResolveOptions {
        http_auth_challenge: options.http_auth_challenge,
        no_netrc: options.no_netrc,
        http_user: options.http_user.clone(),
        http_passwd: options.http_passwd.clone(),
        ftp_user: options.ftp_user.clone(),
        ftp_passwd: options.ftp_passwd.clone(),
        proxy_user,
        proxy_passwd,
    }
}

pub(crate) fn factory_from_options(options: &DownloadOptions) -> AuthConfigFactory {
    let netrc_path = if options.no_netrc {
        None
    } else {
        options
            .netrc_path
            .clone()
            .or_else(crate::http::find_netrc_file)
    };
    factory_from_netrc(netrc_path.as_deref(), options.no_netrc)
}
