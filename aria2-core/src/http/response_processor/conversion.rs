//! Conversion helper from HttpResponseHead to HttpResponse.
//!
//! Bridges the header_processor type with the protocol-level HttpResponse
//! type expected by the skip_response handler.

use crate::http::header_processor::HttpResponseHead;

/// Convert an `HttpResponseHead` (from header_processor) into the
/// `aria2_protocol::http::response::HttpResponse` type expected by
/// the skip_response handler.
pub(crate) fn response_head_to_http_response(
    head: &HttpResponseHead,
) -> aria2_protocol::http::response::HttpResponse {
    let headers: Vec<(String, String)> = head
        .iter_headers()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();

    aria2_protocol::http::response::HttpResponse {
        status_code: head.status_code,
        status_text: head.reason_phrase.clone(),
        headers,
        body: Vec::new(),
    }
}
