use tracing::debug;

use crate::http::response::is_redirect_status;

#[derive(Debug, Clone, PartialEq)]
pub enum RedirectPolicy {
    Follow,
    Limit(usize),
    None,
}

impl Default for RedirectPolicy {
    fn default() -> Self {
        Self::Limit(5)
    }
}

pub struct RedirectHandler;

impl RedirectHandler {
    pub fn should_follow_redirect(
        status_code: u16,
        method: &str,
        current_redirects: usize,
        max_redirects: usize,
    ) -> Option<RedirectAction> {
        if current_redirects >= max_redirects {
            debug!("Maximum redirect limit reached: {}", max_redirects);
            return None;
        }

        if !is_redirect_status(status_code) {
            return None;
        }

        match status_code {
            300 => Some(RedirectAction::FollowKeepMethod),
            301 | 302 if method.eq_ignore_ascii_case("POST") => {
                Some(RedirectAction::FollowChangeToGet)
            }
            301 | 302 => Some(RedirectAction::FollowKeepMethod),
            303 if method.eq_ignore_ascii_case("HEAD") => Some(RedirectAction::FollowKeepMethod),
            303 => Some(RedirectAction::FollowChangeToGet),
            307 | 308 => Some(RedirectAction::FollowKeepMethod),
            _ => None,
        }
    }

    pub fn resolve_redirect_url(current_url: &str, location: &str) -> Result<String, String> {
        let location = location.trim();
        if location.is_empty() {
            return Err("Location header is empty".to_string());
        }

        let base = url::Url::parse(current_url)
            .map_err(|error| format!("Invalid current URL: {error}"))?;
        base.join(location)
            .map(String::from)
            .map_err(|error| format!("Invalid redirect URL: {error}"))
    }

    pub fn sanitize_redirect_url(url: &str) -> Result<String, String> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("Unsafe redirect URL protocol: {}", url));
        }
        Ok(url.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectAction {
    FollowKeepMethod,
    FollowChangeToGet,
}
