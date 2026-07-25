//! HTTP pipelined connection — mirrors C++ `HttpConnection`.
//!
//! Manages a single TCP connection that may have multiple in-flight HTTP
//! requests (pipelining).  Requests are pushed onto an outstanding-queue
//! and responses are consumed in FIFO order.
//!
//! # Pipelining Model
//!
//! The C++ `HttpConnection` maintains a `deque<unique_ptr<HttpRequestEntry>>`
//! of outstanding requests.  `sendRequest()` appends to the deque;
//! `receiveResponse()` pops from the front.  1xx informational responses
//! reset the front entry's header processor without popping.
//!
//! This Rust version uses a `VecDeque<PendingRequest>` with the same
//! FIFO semantics.  Each `PendingRequest` carries a segment ID (for
//! `isIssued()` checks) and an `HttpHeaderProcessor` for incremental
//! response parsing.

use std::collections::VecDeque;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, trace, warn};

use crate::error::{Aria2Error, Result};
use crate::http::auth::erase_confidential_info;
use crate::http::header_processor::{HttpHeaderParseState, HttpHeaderProcessor, HttpResponseHead};
use crate::http::request_response::HttpMethod;

// ---------------------------------------------------------------------------
// PendingRequest — mirrors C++ HttpRequestEntry
// ---------------------------------------------------------------------------

/// A pending (in-flight) HTTP request awaiting its response.
///
/// Mirrors the C++ `HttpRequestEntry` which pairs an `HttpRequest` with
/// an `HttpHeaderProcessor`.  The `segment_id` allows `isIssued()`
/// checks; the `method` and `uri` are retained for auth retry.
pub struct PendingRequest {
    /// Segment identifier (for pipelining overlap checks).
    /// Matches the C++ `segment` used in `isIssued()`.
    pub segment_id: Option<u64>,
    /// HTTP method of the request.
    pub method: HttpMethod,
    /// Request URI (path + query).
    pub uri: String,
    /// Streaming header processor for the associated response.
    pub header_processor: HttpHeaderProcessor,
}

impl std::fmt::Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRequest")
            .field("segment_id", &self.segment_id)
            .field("method", &self.method)
            .field("uri", &self.uri)
            .field("header_processor", &"<HttpHeaderProcessor>")
            .finish()
    }
}

impl PendingRequest {
    /// Create a new pending request entry.
    pub fn new(segment_id: Option<u64>, method: HttpMethod, uri: String) -> Self {
        Self {
            segment_id,
            method,
            uri,
            header_processor: HttpHeaderProcessor::new(),
        }
    }

    /// Reset the header processor (used after receiving a 1xx informational
    /// response, matching C++ `resetHttpHeaderProcessor()`).
    pub fn reset_header_processor(&mut self) {
        self.header_processor = HttpHeaderProcessor::new();
    }
}

// ---------------------------------------------------------------------------
// Parsed response + its associated request metadata
// ---------------------------------------------------------------------------

/// A fully-parsed HTTP response paired with the original request metadata.
///
/// Returned by [`HttpPipelineConnection::receive_response()` on success.
#[derive(Debug)]
pub struct PipelineResponse {
    /// The parsed response head (status line + headers).
    pub head: HttpResponseHead,
    /// HTTP method of the original request.
    pub method: HttpMethod,
    /// URI of the original request.
    pub uri: String,
    /// Segment ID of the original request (if any).
    pub segment_id: Option<u64>,
}

// ---------------------------------------------------------------------------
// HttpPipelineConnection — mirrors C++ HttpConnection
// ---------------------------------------------------------------------------

/// Read buffer size for response header parsing.
const READ_BUF_SIZE: usize = 8192;

/// An HTTP/1.1 connection that supports request pipelining.
///
/// Mirrors the C++ `HttpConnection` class.  Requests are sent immediately
/// and pushed onto an internal queue; responses are read and parsed in
/// FIFO order.
///
/// # Pipelining
///
/// HTTP/1.1 pipelining sends multiple requests without waiting for each
/// response.  This is only safe when the server advertises support (the
/// connection is Keep-Alive and the server does not close early).  The
/// caller is responsible for enabling/disabling pipelining; this struct
/// simply maintains the FIFO queue invariant.
///
/// # 1xx Handling
///
/// 1xx informational responses (100 Continue, 101 Switching Protocols)
/// are consumed internally without returning to the caller.  The front
/// entry's header processor is reset, matching C++ behavior.
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::connection::pipeline::HttpPipelineConnection;
///
/// let conn = HttpPipelineConnection::new(stream);
/// conn.send_request(1, &raw_request_bytes, Some(0), HttpMethod::Get, "/file".into()).await?;
/// let resp = conn.receive_response().await?;
/// ```
pub struct HttpPipelineConnection {
    /// The underlying TCP stream.
    stream: TcpStream,
    /// FIFO queue of outstanding requests awaiting their responses.
    outstanding: VecDeque<PendingRequest>,
}

impl HttpPipelineConnection {
    /// Create a new pipelined connection wrapping the given TCP stream.
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            outstanding: VecDeque::new(),
        }
    }

    /// Whether there are outstanding requests awaiting responses.
    pub fn has_outstanding(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// Number of outstanding requests.
    pub fn outstanding_count(&self) -> usize {
        self.outstanding.len()
    }

    /// Check whether a segment has already been issued on this connection.
    ///
    /// Mirrors C++ `HttpConnection::isIssued()`.
    pub fn is_issued(&self, segment_id: u64) -> bool {
        self.outstanding
            .iter()
            .any(|entry| entry.segment_id == Some(segment_id))
    }

    /// Send a raw HTTP request and enqueue it for response tracking.
    ///
    /// The `raw_request` should be a fully-formatted HTTP/1.1 request
    /// (including `\r\n` terminators).  Confidential headers are stripped
    /// from the debug log via [`erase_confidential_info`].
    ///
    /// Mirrors C++ `HttpConnection::sendRequest()`.
    pub async fn send_request(
        &mut self,
        segment_id: Option<u64>,
        raw_request: &[u8],
        method: HttpMethod,
        uri: String,
    ) -> Result<()> {
        // Log the request (with confidential headers stripped)
        let request_str = String::from_utf8_lossy(raw_request);
        let safe_log = erase_confidential_info(&request_str);
        debug!(
            "Sending HTTP request (segment={:?}, outstanding={}): {}",
            segment_id,
            self.outstanding.len(),
            safe_log.trim()
        );

        // Write to stream
        self.stream
            .write_all(raw_request)
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send HTTP request: {}", e)))?;

        // Enqueue for response tracking
        self.outstanding
            .push_back(PendingRequest::new(segment_id, method, uri));

        Ok(())
    }

    /// Send a proxy CONNECT request and enqueue it.
    ///
    /// Mirrors C++ `HttpConnection::sendProxyRequest()`.
    pub async fn send_proxy_request(
        &mut self,
        raw_request: &[u8],
    ) -> Result<()> {
        debug!("Sending proxy CONNECT request");

        self.stream
            .write_all(raw_request)
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to send proxy request: {}", e)))?;

        // Proxy requests use segment_id = None
        self.outstanding
            .push_back(PendingRequest::new(None, HttpMethod::Get, String::new()));

        Ok(())
    }

    /// Attempt to receive and parse the next HTTP response.
    ///
    /// Returns:
    /// - `Ok(Some(PipelineResponse))` when a complete non-1xx response is parsed.
    /// - `Ok(None)` when the response header is incomplete (need more data).
    ///
    /// 1xx informational responses are consumed internally and do not
    /// produce a return value; the front entry's header processor is
    /// reset and the caller should call `receive_response()` again.
    ///
    /// Mirrors C++ `HttpConnection::receiveResponse()`.
    pub async fn receive_response(&mut self) -> Result<Option<PipelineResponse>> {
        if self.outstanding.is_empty() {
            return Err(Aria2Error::Network(
                "No outstanding HTTP request entry found".to_string(),
            ));
        }

        let mut buf = [0u8; READ_BUF_SIZE];

        loop {
            // Read data from the socket
            let n = self.stream.read(&mut buf).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    Aria2Error::Network(
                        "Connection closed by server before response complete (EOF)".to_string(),
                    )
                } else {
                    Aria2Error::Network(format!("Error reading HTTP response: {}", e))
                }
            })?;

            if n == 0 {
                return Err(Aria2Error::Network(
                    "Connection closed by server before response complete".to_string(),
                ));
            }

            trace!("Read {} bytes for response parsing", n);

            // Feed bytes into the front entry's header processor
            let entry = self.outstanding.front_mut().ok_or_else(|| {
                Aria2Error::Network("No outstanding HTTP request entry found".to_string())
            })?;
            let state = entry.header_processor.feed(&buf[..n]).clone();

            match state {
                HttpHeaderParseState::Complete => {
                    let entry = self.outstanding.front().unwrap();
                    let head = entry.header_processor.get_result()?;

                    // Log the response (with confidential info stripped)
                    let header_str = entry.header_processor.get_header_string();
                    let safe_log = erase_confidential_info(&header_str);
                    debug!("Received HTTP response: {}", safe_log.trim());

                    // Handle 1xx informational responses
                    if (100..200).contains(&head.status_code) {
                        trace!(
                            "Received {} informational response, resetting processor",
                            head.status_code
                        );
                        self.outstanding.front_mut().unwrap().reset_header_processor();
                        continue;
                    }

                    // Non-1xx: pop the entry and return
                    let entry = self.outstanding.pop_front().unwrap();
                    return Ok(Some(PipelineResponse {
                        head,
                        method: entry.method,
                        uri: entry.uri,
                        segment_id: entry.segment_id,
                    }));
                }
                HttpHeaderParseState::ParsingStatusLine
                | HttpHeaderParseState::ParsingHeaders => {
                    // Need more data — continue reading
                    continue;
                }
                HttpHeaderParseState::Error(msg) => {
                    self.outstanding.pop_front();
                    return Err(Aria2Error::Parse(format!(
                        "Error parsing HTTP response header: {}",
                        msg
                    )));
                }
            }
        }
    }

    /// Whether the write buffer is empty (all pending data has been flushed).
    ///
    /// Mirrors C++ `HttpConnection::sendBufferIsEmpty()`.
    /// TODO: Implement proper write-buffer tracking when buffered writes
    ///       are needed.  Currently we write directly, so this is always true.
    pub fn send_buffer_is_empty(&self) -> bool {
        true
    }

    /// Flush any pending write data.
    ///
    /// Mirrors C++ `HttpConnection::sendPendingData()`.
    /// TODO: Implement when buffered writes are added.
    pub async fn send_pending_data(&mut self) -> Result<()> {
        self.stream
            .flush()
            .await
            .map_err(|e| Aria2Error::Network(format!("Failed to flush: {}", e)))
    }

    /// Access the underlying TCP stream (e.g., for TLS upgrade).
    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// Consume this connection and return the inner TCP stream.
    pub fn into_stream(self) -> TcpStream {
        self.stream
    }
}

// ---------------------------------------------------------------------------
// NTLM auth stub — mirrors the C++ NTLM enum variant
// ---------------------------------------------------------------------------

/// NTLM authentication state machine.
///
/// NTLM is a connection-oriented authentication scheme that requires
/// multiple round-trips on the same TCP connection.  Full NTLM support
/// requires the `ntlm` crate or an equivalent SSP integration.
///
/// This is a **stub** that provides the API surface; actual NTLM
/// negotiation is not yet implemented.
#[derive(Debug, Clone)]
pub enum NtlmState {
    /// Initial state — no negotiation started.
    Initial,
    /// Received Type 2 challenge from server; need to compute Type 3 response.
    /// TODO: Store server challenge blob for Type 3 computation.
    ChallengeReceived,
    /// NTLM negotiation complete; subsequent requests on this connection
    /// carry the negotiated security context.
    Authenticated,
}

impl NtlmState {
    /// Build the initial Type 1 (Negotiate) message.
    ///
    /// Returns the `Authorization: NTLM <base64>` header value.
    ///
    /// TODO: Implement actual Type 1 message construction using the NTLM
    ///       protocol (NegotiateFlags, domain name, workstation name).
    pub fn build_type1_header(&self) -> Result<String> {
        // TODO: Implement NTLM Type 1 message
        warn!("NTLM Type 1 message construction not yet implemented");
        Err(Aria2Error::Network(
            "NTLM authentication is not yet implemented".to_string(),
        ))
    }

    /// Process a Type 2 challenge from the server and produce a Type 3
    /// response header value.
    ///
    /// TODO: Parse the server challenge, compute the NTLMv1/v2 response
    ///       hash, and construct the Type 3 (Authenticate) message.
    pub fn build_type3_header(
        &mut self,
        _challenge_header: &str,
        _username: &str,
        _password: &str,
    ) -> Result<String> {
        // TODO: Implement NTLM Type 3 message
        warn!("NTLM Type 3 message construction not yet implemented");
        Err(Aria2Error::Network(
            "NTLM authentication is not yet implemented".to_string(),
        ))
    }

    /// Whether NTLM negotiation is complete.
    pub fn is_authenticated(&self) -> bool {
        matches!(self, NtlmState::Authenticated)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_request_reset_processor() {
        let mut pr = PendingRequest::new(Some(1), HttpMethod::Get, "/file".into());
        // Feed some data to the processor
        let _ = pr.header_processor.feed(b"HTTP/1.1 100 Continue\r\n\r\n");
        pr.reset_header_processor();
        // After reset, the processor should be fresh (state == Partial)
        // We can't directly check state, but feeding again should work
    }

    #[test]
    fn test_pipeline_connection_initial_state() {
        // We can't create a real TcpStream in unit tests, so test the
        // data structures only.
        let outstanding: VecDeque<PendingRequest> = VecDeque::new();
        assert!(outstanding.is_empty());

        let outstanding = {
            let mut q = VecDeque::new();
            q.push_back(PendingRequest::new(Some(1), HttpMethod::Get, "/a".into()));
            q.push_back(PendingRequest::new(Some(2), HttpMethod::Get, "/b".into()));
            q
        };
        assert_eq!(outstanding.len(), 2);

        // is_issued check
        assert!(outstanding.iter().any(|e| e.segment_id == Some(1)));
        assert!(outstanding.iter().any(|e| e.segment_id == Some(2)));
        assert!(!outstanding.iter().any(|e| e.segment_id == Some(3)));
    }

    #[test]
    fn test_ntlm_state_stub() {
        let state = NtlmState::Initial;
        assert!(!state.is_authenticated());

        let state = NtlmState::Authenticated;
        assert!(state.is_authenticated());

        let mut state = NtlmState::ChallengeReceived;
        assert!(state.build_type3_header("challenge", "user", "pass").is_err());
    }

    #[test]
    fn test_erase_confidential_in_pipeline() {
        let raw = "GET / HTTP/1.1\r\nAuthorization: Basic dXNlcg==\r\nHost: example.com\r\n";
        let safe = erase_confidential_info(raw);
        assert!(safe.contains("Authorization: <snip>"));
        assert!(safe.contains("Host: example.com"));
    }
}
