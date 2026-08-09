//! HTTP response processor implementation.
//!
//! Processes HTTP response headers and determines the next download action.
//! This is the Rust equivalent of C++ `HttpResponseCommand::executeInternal()`.
//! Instead of creating new command objects, it returns `ResponseProcessResult`
//! values that the caller interprets.

use tracing::{debug, info};

use crate::error::{Aria2Error, Result};
use crate::http::header_processor::HttpResponseHead;
use crate::http::metalink_http::MetalinkHttpParser;
use crate::http::request_response::HttpMethod;
use crate::http::skip_response::{HttpSkipResponseHandler, MAX_REDIRECT_COUNT, SkipResponseResult};

use super::connection::{should_inflate_content_encoding, supports_persistent_connection};
use super::conversion;
use super::filename::determine_filename;
use super::range;
use super::types::{ResponseProcessResult, ResponseProcessorConfig};
use super::validate::{ValidateRequestContext, validate_response};

/// Processes HTTP response headers and determines the next download action.
///
/// This is the Rust equivalent of C++ `HttpResponseCommand::executeInternal()`.
/// Instead of creating new command objects, it returns `ResponseProcessResult`
/// values that the caller interprets.
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::response_processor::{HttpResponseProcessor, ResponseProcessorConfig};
/// use aria2_core::http::header_processor::HttpResponseHead;
///
/// let processor = HttpResponseProcessor::new(ResponseProcessorConfig::default());
/// let result = processor.process(&response_head, HttpMethod::Get, &request_url, None, true, false, true, false)?;
/// match result {
///     ResponseProcessResult::DownloadReady { filename, .. } => { /* start download */ }
///     ResponseProcessResult::Redirect(info) => { /* follow redirect */ }
///     _ => { /* handle other cases */ }
/// }
/// ```
pub struct HttpResponseProcessor {
    config: ResponseProcessorConfig,
}

impl HttpResponseProcessor {
    /// Create a new processor with the given configuration.
    pub fn new(config: ResponseProcessorConfig) -> Self {
        Self { config }
    }

    /// Create a processor with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ResponseProcessorConfig::default())
    }

    /// Process an HTTP response header and determine the next action.
    ///
    /// This is the main entry point, equivalent to C++
    /// `HttpResponseCommand::executeInternal()`.
    ///
    /// # Arguments
    ///
    /// * `response_head` - Parsed HTTP response headers.
    /// * `request_method` - The HTTP method of the original request.
    /// * `request_url` - The URL of the original request (for filename derivation).
    /// * `requested_range` - The (start, end) range requested, if any.
    /// * `piece_storage_initialized` - Whether piece storage has already been set up.
    /// * `is_unique_protocol` - Whether all URIs use the same protocol.
    /// * `accept_metalink` - Whether Metalink/HTTP processing is enabled.
    /// * `conditional_request` - Whether the request included conditional GET
    ///   headers (`If-Modified-Since` or `If-None-Match`).
    ///
    /// # Returns
    ///
    /// A `ResponseProcessResult` indicating the next action for the download engine.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &self,
        response_head: &HttpResponseHead,
        request_method: HttpMethod,
        request_url: &str,
        requested_range: Option<(u64, u64)>,
        piece_storage_initialized: bool,
        is_unique_protocol: bool,
        accept_metalink: bool,
        conditional_request: bool,
    ) -> Result<ResponseProcessResult> {
        // --- Protocol validation (mirrors C++ HttpResponse::validateResponse()) ---
        let validate_ctx = ValidateRequestContext {
            conditional_request,
            requested_range,
            expected_entity_length: self.config.expected_entity_length,
        };
        validate_response(response_head, &validate_ctx)?;

        let status_code = response_head.status_code;

        // --- Connection persistence ---
        let supports_persistent = supports_persistent_connection(response_head);

        // --- 304 Not Modified ---
        if status_code == 304 {
            return self.handle_not_modified(response_head);
        }

        // --- Metalink/HTTP and Digest processing (only before piece storage init) ---
        let mut metalink_uris = Vec::new();
        let mut digests = Vec::new();

        if !piece_storage_initialized {
            // Metalink/HTTP Link headers
            if accept_metalink && response_head.header("link").is_some() {
                let metalink_result = MetalinkHttpParser::parse_response(
                    response_head,
                    &self.config.metalink_location,
                );
                for link in &metalink_result.links {
                    metalink_uris.push(link.uri.clone());
                    debug!(uri = %link.uri, "Adding Metalink/HTTP URI");
                }
                digests = metalink_result.digests;
            }

            // Digest header (standalone, not from Link)
            if response_head.header("digest").is_some() && digests.is_empty() {
                // When there's no Link header but there is a Digest header,
                // parse just the Digest values. Use header_all to get all
                // Digest header values (matches C++ getDigest equalRange).
                for dv in response_head.header_all("digest") {
                    digests.extend(MetalinkHttpParser::parse_digest_header(dv));
                }
            }
        }

        // --- Non-2xx status codes (3xx/4xx/5xx) ---
        if status_code >= 300 {
            return self.handle_non_success(
                response_head,
                request_method,
                request_url,
                status_code,
            );
        }

        // --- 2xx: Unique protocol URI cleanup ---
        // C++ aria2: if (fe->isUniqueProtocol()) {
        //   uri_split_result us; uri_split(&us, uri.c_str());
        //   std::string host = getFieldString(us, USR_HOST, uri.c_str());
        //   fe->removeURIWhoseHostnameIs(host);
        // }
        // We pass the flag to the caller so it can execute the cleanup
        // at the RequestGroup level (since the processor doesn't have
        // direct access to FileEntry).

        // --- 2xx: Determine download parameters ---
        let entity_length = range::compute_entity_length(response_head);
        let filename = determine_filename(
            response_head,
            request_url,
            self.config.content_disposition_default_utf8,
        );
        let content_type = response_head.header("content-type").map(|s| s.to_string());
        let content_range = range::parse_content_range(response_head);
        let chunked = range::is_chunked_transfer_encoding(response_head);
        let inflate_required =
            should_inflate_content_encoding(response_head, self.config.accept_gzip);

        // --- HEAD -> GET switch detection ---
        let switch_head_to_get = request_method == HttpMethod::Head;

        // --- Last-Modified header extraction ---
        // Per C++ aria2's `updateLastModifiedTime()`: when the `remote-time`
        // option is enabled, the file's mtime is set to the server's
        // Last-Modified time after download completion.
        let last_modified = response_head.header("last-modified").map(|s| s.to_string());

        // --- Determine knows_total_length ---
        let knows_total_length = if entity_length == 0 || inflate_required {
            // When inflate is required or entity_length is 0, the effective
            // total length may be unknown. Per C++ logic:
            // - If GET and (entity_length != 0 or no Content-Length header),
            //   mark total length as unknown.
            if request_method == HttpMethod::Get
                && (entity_length != 0 || response_head.header("content-length").is_none())
            {
                false
            } else {
                // entity_length == 0 with explicit Content-Length: 0 means
                // the server explicitly says the file is zero-length.
                entity_length == 0 && response_head.header("content-length").is_some()
            }
        } else {
            true
        };

        // --- Log ---
        info!(
            status = status_code,
            entity_length,
            filename = %filename,
            inflate = inflate_required,
            chunked,
            persistent = supports_persistent,
            "HTTP 2xx response processed"
        );

        Ok(ResponseProcessResult::DownloadReady {
            filename,
            entity_length,
            content_type,
            inflate_required,
            chunked,
            knows_total_length,
            supports_persistent_connection: supports_persistent,
            switch_head_to_get,
            metalink_uris,
            digests,
            content_range,
            last_modified,
            is_unique_protocol,
        })
    }

    /// Handle 304 Not Modified response.
    ///
    /// Per C++ behavior: set file entry length, mark all pieces done,
    /// set checksum verified, determine filename if path is empty.
    fn handle_not_modified(
        &self,
        response_head: &HttpResponseHead,
    ) -> Result<ResponseProcessResult> {
        let entity_length = range::compute_entity_length(response_head);
        info!(entity_length, "304 Not Modified: file already current");
        Ok(ResponseProcessResult::NotModified { entity_length })
    }

    /// Handle non-success status codes (3xx/4xx/5xx) by delegating to
    /// the skip_response handler.
    fn handle_non_success(
        &self,
        response_head: &HttpResponseHead,
        request_method: HttpMethod,
        request_url: &str,
        status_code: u16,
    ) -> Result<ResponseProcessResult> {
        // Build a skip_response handler and delegate to it
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT)
            .with_http_auth_challenge(true)
            .with_max_file_not_found(0)
            .with_retry_wait(5);

        // Convert HttpResponseHead into the HttpResponse type expected
        // by the skip_response handler.
        let http_response = conversion::response_head_to_http_response(response_head);

        let current_url = url::Url::parse(request_url).map_err(|e| {
            Aria2Error::Parse(format!("Invalid request URL '{}': {}", request_url, e))
        })?;

        let skip_result = handler.handle(&http_response, request_method, &current_url, 0)?;

        match skip_result {
            SkipResponseResult::Redirect(info) => Ok(ResponseProcessResult::Redirect(info)),
            SkipResponseResult::AuthChallenge(challenge) => {
                Ok(ResponseProcessResult::AuthChallenge(challenge))
            }
            SkipResponseResult::RetryableError {
                status_code,
                message,
            } => Ok(ResponseProcessResult::Error {
                status_code,
                message,
            }),
            SkipResponseResult::FatalError {
                status_code,
                message,
            } => Ok(ResponseProcessResult::Error {
                status_code,
                message,
            }),
            SkipResponseResult::Consumed => Ok(ResponseProcessResult::Error {
                status_code,
                message: format!("HTTP error: {}", status_code),
            }),
        }
    }
}
