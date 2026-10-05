use crate::engine::http::cookie_helper::CookieHelper;
use crate::engine::http::sequential_download::PreparedHttpResponse;
use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request::HttpMethod;
use crate::http::{
    AuthChallengeResult, AuthConfigFactory, AuthResolveOptions, AuthScheme, HttpAuthChallenge,
    HttpSkipResponseHandler,
};
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    async fn retry_get_after_auth_challenge(
        &self,
        response: reqwest::Response,
        current_url: &reqwest::Url,
        cookie_helper: &CookieHelper,
        auth_factory: &mut AuthConfigFactory,
        auth_options: &AuthResolveOptions,
        authentication_used: bool,
    ) -> reqwest::Response {
        let status_code = response.status().as_u16();
        if status_code != 401 && status_code != 407 {
            return response;
        }

        let is_proxy = status_code == 407;
        let header_name = if is_proxy {
            "proxy-authenticate"
        } else {
            "www-authenticate"
        };
        let auth_header = response
            .headers()
            .get(header_name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let scheme = auth_header
            .as_deref()
            .and_then(AuthScheme::from_header)
            .or((!authentication_used).then_some(AuthScheme::Basic));
        let Some(scheme) = scheme else {
            return response;
        };

        let challenge = HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: auth_header
                .as_deref()
                .map(HttpSkipResponseHandler::extract_realm)
                .unwrap_or_default(),
            is_proxy,
            digest_challenge: (scheme == AuthScheme::Digest)
                .then(|| {
                    auth_header
                        .as_deref()
                        .and_then(|header| DigestAuthChallenge::parse(header).ok())
                })
                .flatten(),
        };

        let AuthChallengeResult::RetryWithAuth {
            authorization_header,
            is_proxy,
        } = crate::http::handle_auth_challenge(
            &challenge,
            auth_factory,
            current_url,
            auth_options,
            HttpMethod::Get,
            authentication_used,
            1,
        )
        else {
            return response;
        };

        let header_name = if is_proxy {
            "Proxy-Authorization"
        } else {
            "Authorization"
        };
        let cookie_header = cookie_helper.build_cookie_header_from_url(current_url);
        let request = self.request_policy.apply(
            self.client.get(current_url.as_str()),
            (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
            &[(header_name.to_string(), authorization_header)],
        );
        let Ok(retry_response) = request.send().await else {
            return response;
        };
        cookie_helper.extract_and_store_cookies(current_url.as_str(), &retry_response);

        // Let the normal response loop handle redirects and HTTP errors. A
        // second auth challenge remains owned by the established downloader.
        if retry_response.status().as_u16() == 401 || retry_response.status().as_u16() == 407 {
            response
        } else {
            retry_response
        }
    }

    pub(super) async fn send_head_with_redirects(&self, uri: &str) -> Option<PreparedHttpResponse> {
        let mut current_url = reqwest::Url::parse(uri).ok()?;
        let cookie_helper = self.create_cookie_helper();
        let initial_scheme = current_url.scheme().to_owned();
        let options = self.group.recover().options_arc();
        let auth_context = crate::engine::http::auth::from_options(&options, &initial_scheme);
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);

        for _ in 0..=crate::http::skip_response::MAX_REDIRECT_COUNT {
            let cookie_header = cookie_helper.build_cookie_header_from_url(&current_url);
            let authorization =
                auth_factory.resolve_basic_authorization(&current_url, &auth_options);
            let request = self.request_policy.apply_with_basic_auth(
                self.client.head(current_url.as_str()),
                (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
                &[],
                authorization.as_deref(),
            );
            let response = request.send().await.ok()?;
            cookie_helper.extract_and_store_cookies(current_url.as_str(), &response);

            let status_code = response.status().as_u16();
            if !matches!(status_code, 300..=303 | 307 | 308) {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())?;
            current_url = current_url.join(location).ok()?;
        }

        None
    }

    /// Read the first ordinary GET response before finalizing an inferred
    /// output path. The sequential downloader can reuse this response body;
    /// a later concurrent decision may deliberately discard it and issue
    /// range requests instead.
    pub(super) async fn send_get_with_redirects(&self, uri: &str) -> Option<PreparedHttpResponse> {
        let mut current_url = reqwest::Url::parse(uri).ok()?;
        let cookie_helper = self.create_cookie_helper();
        let initial_scheme = current_url.scheme().to_owned();
        let options = self.group.recover().options_arc();
        let auth_context = crate::engine::http::auth::from_options(&options, &initial_scheme);
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);

        for _ in 0..=crate::http::skip_response::MAX_REDIRECT_COUNT {
            let cookie_header = cookie_helper.build_cookie_header_from_url(&current_url);
            let authorization =
                auth_factory.resolve_basic_authorization(&current_url, &auth_options);
            let request = self.request_policy.apply_with_basic_auth(
                self.client.get(current_url.as_str()),
                (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
                &[],
                authorization.as_deref(),
            );
            let mut response = request.send().await.ok()?;
            cookie_helper.extract_and_store_cookies(current_url.as_str(), &response);
            response = self
                .retry_get_after_auth_challenge(
                    response,
                    &current_url,
                    &cookie_helper,
                    &mut auth_factory,
                    &auth_options,
                    authorization.is_some(),
                )
                .await;

            if !crate::http::response::is_redirect_status(response.status().as_u16()) {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            }

            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            };

            let Ok(next_url) = current_url.join(location) else {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            };
            self.group.recover_mut().add_redirect_uri(next_url.as_str());
            current_url = next_url;
        }

        None
    }
}
