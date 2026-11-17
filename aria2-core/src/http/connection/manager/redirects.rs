use std::collections::HashSet;
use url::Url;

use aria2_protocol::http::response::is_redirect_status;

use crate::error::{Aria2Error, Result};

use super::super::types::HttpResponse;
use super::HttpConnectionManager;

impl HttpConnectionManager {
    // ==================== Redirect Following ====================

    /// Follow HTTP redirects with circular detection.
    pub fn follow_redirects(
        &self,
        response: &HttpResponse,
        current_url: &Url,
        redirect_chain: &HashSet<Url>,
        redirect_count: u32,
    ) -> Result<Url> {
        if !is_redirect_status(response.status_code) {
            return Err(Aria2Error::Parse(format!(
                "Non-redirect response code: {}",
                response.status_code
            )));
        }
        if redirect_count >= self.max_redirects {
            return Err(Aria2Error::Network(format!(
                "Max redirect count exceeded: {}",
                self.max_redirects
            )));
        }
        let location = response
            .location()
            .ok_or_else(|| Aria2Error::Parse("Missing Location header".to_string()))?;
        let new_url = current_url
            .join(location)
            .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;
        if redirect_chain.contains(&new_url) {
            return Err(Aria2Error::Network(format!(
                "Circular redirect detected: {}",
                new_url
            )));
        }
        tracing::info!(
            "Following redirect: {} -> {} ({}/{})",
            current_url,
            new_url,
            redirect_count + 1,
            self.max_redirects
        );
        Ok(new_url)
    }

    /// Iteratively follow HTTP redirects with loop detection.
    pub async fn follow_redirects_iterative<F, Fut>(
        &self,
        initial_url: &Url,
        mut get_response: F,
    ) -> Result<HttpResponse>
    where
        F: FnMut(&Url) -> Fut,
        Fut: std::future::Future<Output = Result<HttpResponse>>,
    {
        const MAX_REDIRECTS: u8 = crate::constants::HTTP_DEFAULT_MAX_REDIRECTS as u8;
        let mut current_url = initial_url.clone();
        let mut seen_urls = HashSet::<String>::new();

        for iteration in 0..MAX_REDIRECTS {
            let url_str = current_url.to_string();
            if !seen_urls.insert(url_str.clone()) {
                return Err(Aria2Error::Network(format!(
                    "Redirect loop detected: {}",
                    url_str
                )));
            }
            let resp = get_response(&current_url).await?;
            if !is_redirect_status(resp.status_code) {
                return Ok(resp);
            }
            let location = resp.location().ok_or_else(|| {
                Aria2Error::Network("Missing Location header in redirect response".into())
            })?;
            current_url = current_url
                .join(location)
                .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;
            tracing::info!(
                "Following redirect: iteration {}/{}",
                iteration + 1,
                MAX_REDIRECTS
            );
        }

        Err(Aria2Error::Network(format!(
            "Too many redirects (>{}), last URL: {}",
            MAX_REDIRECTS, current_url
        )))
    }

    // ==================== Range Headers ====================
}
