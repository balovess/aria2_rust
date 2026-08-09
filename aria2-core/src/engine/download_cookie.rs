use std::sync::Arc;

use crate::http::cookie::Cookie;
use crate::http::cookie_storage::CookieStorage;

#[derive(Clone)]
pub struct CookieHelper {
    cookie_storage: Arc<CookieStorage>,
    cookie_file: Option<String>,
}

impl CookieHelper {
    pub fn new(cookie_storage: Arc<CookieStorage>, cookie_file: Option<String>) -> Self {
        Self {
            cookie_storage,
            cookie_file,
        }
    }

    pub fn build_cookie_header(&self, uri: &str) -> Option<String> {
        let url_parsed = reqwest::Url::parse(uri).ok()?;
        let host = url_parsed.host_str()?;
        let path = url_parsed.path();
        let secure = url_parsed.scheme() == "https";
        let hdr = self.cookie_storage.to_header_string(host, path, secure);
        if hdr.is_empty() { None } else { Some(hdr) }
    }

    pub fn build_cookie_header_from_url(&self, url: &reqwest::Url) -> String {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let secure = url.scheme() == "https";
        self.cookie_storage.to_header_string(host, path, secure)
    }

    pub fn extract_and_store_cookies(&self, uri: &str, response: &reqwest::Response) {
        let url_parsed = reqwest::Url::parse(uri).ok();
        if let Some(ref url) = url_parsed {
            let domain = url.host_str().unwrap_or("");
            let default_path = Cookie::default_path(url.path());
            for sc_val in response.headers().get_all("set-cookie").iter() {
                if let Ok(sc_str) = sc_val.to_str()
                    && let Some(c) = Cookie::from_set_cookie_header(sc_str, domain, &default_path)
                {
                    self.cookie_storage.add(c);
                    tracing::debug!("Received Set-Cookie: {}", sc_str);
                }
            }
        }
    }

    pub fn save_cookies_if_configured(&self) {
        if let Some(ref cf) = self.cookie_file {
            if let Err(e) = self.cookie_storage.save_file(std::path::Path::new(cf)) {
                tracing::warn!("Failed to save cookie file {}: {}", cf, e);
            } else {
                tracing::info!("Cookies saved to {}", cf);
            }
        }
    }
}
