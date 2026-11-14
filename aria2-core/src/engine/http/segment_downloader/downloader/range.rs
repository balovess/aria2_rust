use bytes::BytesMut;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use aria2_protocol::http::parse_unsatisfied_content_range_length;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::auth_challenge_handler::{self, AuthChallengeResult};
use crate::http::skip_response::MAX_REDIRECT_COUNT;
use crate::http::{AuthScheme, HttpAuthChallenge};

use super::{
    HttpSegmentDownloader, RangeProbeResult, classify_range_status, validate_content_range,
};

fn response_with_origin_scope(
    response: reqwest::Response,
    url: reqwest::Url,
    scope: &crate::http::request_policy::RedirectOriginScope,
) -> (reqwest::Response, reqwest::Url, bool) {
    let allow_sensitive_headers = scope.allows_sensitive_headers(&url);
    (response, url, allow_sensitive_headers)
}

impl HttpSegmentDownloader {
    /// Send a Range request through the same manual redirect seam as the
    /// sequential downloader. reqwest's client is configured with automatic
    /// redirects disabled so the final URI remains explicit and bounded.
    pub(super) async fn send_range_request(
        &self,
        url: &str,
        range_header: &str,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<(reqwest::Response, reqwest::Url, bool)> {
        let mut current_url = reqwest::Url::parse(url).map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("invalid HTTP Range URL {url}: {error}"),
            })
        })?;
        let mut origin_scope =
            crate::http::request_policy::RedirectOriginScope::new(current_url.clone())
                .with_sensitive_headers_allowed(self.origin_sensitive_headers_allowed);
        let mut redirect_count = 0u32;

        loop {
            self.request_policy.wait_before_request().await;
            let dynamic_cookie_header = self
                .cookie_helper
                .as_ref()
                .map(|helper| helper.build_cookie_header_from_url(&current_url));
            let request_cookie_header = dynamic_cookie_header
                .as_deref()
                .filter(|value| !value.is_empty())
                .or_else(|| (redirect_count == 0).then_some(cookie_header).flatten());
            let allow_origin_auth = origin_scope.allows_sensitive_headers(&current_url);
            let authorization = allow_origin_auth
                .then(|| {
                    self.auth_options.as_ref().and_then(|auth_options| {
                        self.auth_factory
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .resolve_basic_authorization(&current_url, auth_options)
                    })
                })
                .flatten();
            let request = self.request_policy.apply_with_basic_auth_for_origin(
                self.client
                    .get(current_url.as_str())
                    .header("Range", range_header),
                request_cookie_header,
                headers,
                authorization.as_deref(),
                allow_origin_auth,
            );
            let mut response = request.send().await.map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("HTTP Range request failed: {error}"),
                })
            })?;

            let mut status_code = response.status().as_u16();
            let authentication_used = authorization.is_some()
                || (allow_origin_auth && self.request_policy.has_header("Authorization"))
                || (allow_origin_auth
                    && headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case("Authorization")));
            if status_code == 401 || status_code == 407 {
                if status_code == 401 && !allow_origin_auth {
                    return Ok(response_with_origin_scope(
                        response,
                        current_url,
                        &origin_scope,
                    ));
                }
                let Some(auth_options) = &self.auth_options else {
                    return Ok(response_with_origin_scope(
                        response,
                        current_url,
                        &origin_scope,
                    ));
                };
                let is_proxy = status_code == 407;
                if authentication_used
                    || (!is_proxy && !auth_options.http_auth_challenge)
                    || (is_proxy && auth_options.proxy_user.is_none())
                {
                    return Ok(response_with_origin_scope(
                        response,
                        current_url,
                        &origin_scope,
                    ));
                }

                let header_name = if is_proxy {
                    reqwest::header::PROXY_AUTHENTICATE
                } else {
                    reqwest::header::WWW_AUTHENTICATE
                };
                let auth_header = response
                    .headers()
                    .get(header_name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let scheme = auth_header
                    .as_deref()
                    .and_then(AuthScheme::from_header)
                    .or_else(|| (!is_proxy).then_some(AuthScheme::Basic));
                let Some(scheme) = scheme else {
                    return Ok(response_with_origin_scope(
                        response,
                        current_url,
                        &origin_scope,
                    ));
                };
                let challenge = HttpAuthChallenge {
                    scheme: scheme.clone(),
                    realm: auth_header
                        .as_deref()
                        .map(crate::http::skip_response::HttpSkipResponseHandler::extract_realm)
                        .unwrap_or_default(),
                    is_proxy,
                    digest_challenge: if scheme == AuthScheme::Digest {
                        auth_header.as_deref().and_then(|header| {
                            crate::http::digest_auth::DigestAuthChallenge::parse(header).ok()
                        })
                    } else {
                        None
                    },
                };
                let result = {
                    let mut auth_factory = self
                        .auth_factory
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    auth_challenge_handler::handle_auth_challenge(
                        &challenge,
                        &mut auth_factory,
                        &current_url,
                        auth_options,
                        crate::http::request::HttpMethod::Get,
                        authentication_used,
                        1,
                    )
                };
                let AuthChallengeResult::RetryWithAuth {
                    authorization_header,
                    is_proxy,
                } = result
                else {
                    return Ok(response_with_origin_scope(
                        response,
                        current_url,
                        &origin_scope,
                    ));
                };

                let header_name = if is_proxy {
                    "Proxy-Authorization"
                } else {
                    "Authorization"
                };
                let dynamic_cookie_header = self
                    .cookie_helper
                    .as_ref()
                    .map(|helper| helper.build_cookie_header_from_url(&current_url));
                let retry_cookie_header = dynamic_cookie_header
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .or_else(|| cookie_header.filter(|_| redirect_count == 0));
                let retry_request = if is_proxy {
                    self.request_policy.apply_for_origin_with_proxy_auth(
                        self.client
                            .get(current_url.as_str())
                            .header("Range", range_header),
                        retry_cookie_header,
                        &[],
                        allow_origin_auth,
                        Some(&authorization_header),
                    )
                } else {
                    self.request_policy.apply_for_origin(
                        self.client
                            .get(current_url.as_str())
                            .header("Range", range_header),
                        retry_cookie_header,
                        &[(header_name.to_string(), authorization_header)],
                        allow_origin_auth,
                    )
                };
                self.request_policy.wait_before_request().await;
                let retry_response = retry_request.send().await.map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP Range auth retry failed: {error}"),
                    })
                })?;
                if let Some(helper) = &self.cookie_helper {
                    helper.extract_and_store_cookies(current_url.as_str(), &retry_response);
                }
                status_code = retry_response.status().as_u16();
                response = retry_response;
            }
            if !matches!(status_code, 300..=303 | 307 | 308) {
                if let Some(helper) = &self.cookie_helper {
                    helper.extract_and_store_cookies(current_url.as_str(), &response);
                }
                return Ok(response_with_origin_scope(
                    response,
                    current_url,
                    &origin_scope,
                ));
            }

            if let Some(helper) = &self.cookie_helper {
                helper.extract_and_store_cookies(current_url.as_str(), &response);
            }

            if redirect_count >= MAX_REDIRECT_COUNT {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::HttpTooManyRedirects {
                        count: redirect_count,
                    },
                ));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                        message: format!("HTTP {status_code} redirect without Location header"),
                    })
                })?;
            let target_url = current_url.join(location).map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                    message: format!("failed to resolve redirect URL '{location}': {error}"),
                })
            })?;
            origin_scope.follow_redirect(&current_url, &target_url);
            current_url = target_url;
            redirect_count += 1;
        }
    }

    /// Probe byte-range support with the same redirect, cookie, and auth path
    /// used by actual range downloads. `Accept-Ranges` from HEAD is only a
    /// hint; a valid 206 and exact one-byte Content-Range is the evidence that
    /// this resource can be split.
    pub(crate) async fn probe_range_metadata(
        &self,
        url: &str,
        cookie_header: Option<&str>,
    ) -> Result<RangeProbeResult> {
        let (response, effective_url, origin_sensitive_headers_allowed) = self
            .send_range_request(url, "bytes=0-0", cookie_header, &[])
            .await?;

        let version = response.version();
        let status = response.status();
        if !matches!(status.as_u16(), 200 | 206 | 416)
            && let Some(error) = classify_range_status(status, "bytes=0-0")
        {
            return Err(error);
        }
        let content_length = response.content_length().unwrap_or(0);
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok());
        let parsed_range = content_range
            .and_then(crate::http::response_processor::range::parse_content_range_value);
        let supports_range = status.as_u16() == 206
            && parsed_range.is_some_and(|(start, end, total)| start == 0 && end == 0 && total > 0);
        let total_length = if supports_range {
            parsed_range.map(|(_, _, total)| total).unwrap_or(0)
        } else if status.as_u16() == 200 && content_length > 0 {
            content_length
        } else if status.as_u16() == 416 {
            content_range
                .and_then(parse_unsatisfied_content_range_length)
                .unwrap_or(0)
        } else {
            0
        };

        Ok(RangeProbeResult {
            supports_range,
            effective_url: effective_url.to_string(),
            origin_sensitive_headers_allowed,
            total_length,
            version: Some(version),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
        expected_entity_length: u64,
    ) -> Result<bytes::Bytes> {
        if length == 0 {
            return Ok(bytes::Bytes::new());
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request: {} ({})", range_header, url);

        let (response, effective_url, _) = self
            .send_range_request(url, &range_header, cookie_header, headers)
            .await?;

        self.remember_peer(response.remote_addr());
        let status = response.status();
        if let Some(error) = classify_range_status(status, &range_header) {
            return Err(error);
        }
        if status.as_u16() == 206 {
            validate_content_range(&response, offset, length, expected_entity_length)?;
        }

        // Don't pre-allocate the full segment length — it can be very large
        // (16 MB+) and wastes memory if the download fails early.  Start with
        // a reasonable chunk size and let BytesMut grow organically.
        let initial_cap = (length as usize).min(256 * 1024);
        let mut data = BytesMut::with_capacity(initial_cap);
        let mut stream = response.bytes_stream();
        let mut last_reported_progress = 0u64;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    if data.len() as u64 + bytes.len() as u64 > length {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Response exceeded requested range length: expected {}, received more",
                                    length
                                ),
                            },
                        ));
                    }
                    data.extend_from_slice(&bytes);
                    // Report per-chunk progress if a progress channel is provided
                    let downloaded = data.len() as u64;
                    if let Some(tx) = progress_tx
                        && downloaded - last_reported_progress
                            >= constants::PROGRESS_UPDATE_BYTES as u64
                    {
                        let update = ProgressUpdate {
                            completed_bytes: offset + downloaded,
                            download_speed: 0,
                            upload_speed: 0,
                        };
                        // Progress is advisory; avoid stalling the data path
                        // when the bounded snapshot queue is temporarily full.
                        let _ = tx.try_send(update);
                        last_reported_progress = downloaded;
                    }
                }
                Err(e) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Stream read error: {}", e),
                        },
                    ));
                }
            }
        }

        if data.len() as u64 != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        effective_url,
                        length,
                        data.len()
                    ),
                },
            ));
        }

        // Freeze BytesMut to immutable Bytes (zero-cost conversion)
        Ok(data.freeze())
    }
}
