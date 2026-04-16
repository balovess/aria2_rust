//! Mock HTTP Server for E2E testing
//!
#![allow(dead_code)]
//! Provides a lightweight HTTP server that can be configured with custom handlers,
//! supporting auth challenges, gzip responses, chunked encoding, and range requests.

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio::sync::oneshot;

/// Mock HTTP handler type
type HandlerFn = Arc<dyn Fn(&Request<Body>) -> Response<Body> + Send + Sync>;

struct RouteEntry {
    method: Option<String>,
    path_pattern: String,
    handler: HandlerFn,
}

pub struct MockHttpServer {
    addr: SocketAddr,
    routes: Arc<RwLock<Vec<RouteEntry>>>,
    request_log: Arc<RwLock<Vec<RequestLog>>>,
    shutdown_tx: Option<Arc<oneshot::Sender<()>>>,
}

#[derive(Debug, Clone)]
pub struct RequestLog {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl MockHttpServer {
    /// Start a new mock server (auto-binds to random port)
    pub async fn start() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let addr: SocketAddr = "127.0.0.1:0".parse()?;
        let routes: Arc<RwLock<Vec<RouteEntry>>> = Arc::new(RwLock::new(Vec::new()));
        let request_log: Arc<RwLock<Vec<RequestLog>>> = Arc::new(RwLock::new(Vec::new()));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let routes_clone = Arc::clone(&routes);
        let log_clone = Arc::clone(&request_log);

        let make_svc = make_service_fn(move |_conn| {
            let routes = Arc::clone(&routes_clone);
            let log = Arc::clone(&log_clone);
            async move {
                Ok::<_, Infallible>(service_fn(move |req: Request<Body>| {
                    let routes = Arc::clone(&routes);
                    let log = Arc::clone(&log);
                    async move {
                        // Log the request
                        let req_log = RequestLog {
                            method: req.method().to_string(),
                            path: req.uri().path().to_string(),
                            headers: req
                                .headers()
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                                .collect(),
                        };
                        log.write().unwrap().push(req_log);

                        // Find matching route
                        let routes_guard = routes.read().unwrap();
                        let method = req.method().to_string();
                        let path = req.uri().path();

                        for route in routes_guard.iter() {
                            // Check method match
                            if let Some(ref m) = route.method
                                && m != &method
                            {
                                continue;
                            }

                            // Check path match (prefix or exact)
                            if path.starts_with(&route.path_pattern) || route.path_pattern == path {
                                let handler = &route.handler;
                                return Ok::<Response<Body>, Infallible>(handler(&req));
                            }
                        }

                        // No matching route found - return 404
                        Ok(Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::from("Not Found"))
                            .unwrap())
                    }
                }))
            }
        });

        let server = Server::bind(&addr).serve(make_svc);
        let actual_addr = server.local_addr();
        let server_with_shutdown = server.with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });

        // Spawn server in background
        tokio::spawn(async move {
            if let Err(e) = server_with_shutdown.await {
                eprintln!("Mock HTTP server error: {}", e);
            }
        });

        Ok(MockHttpServer {
            addr: actual_addr,
            routes,
            request_log,
            shutdown_tx: Some(Arc::new(shutdown_tx)),
        })
    }

    /// Register GET route handler
    pub fn on_get<F>(&self, pattern: &str, handler: F)
    where
        F: Fn(&Request<Body>) -> Response<Body> + Send + Sync + 'static,
    {
        self.on("GET", pattern, handler)
    }

    /// Register arbitrary method route
    pub fn on<F>(&self, method: &str, pattern: &str, handler: F)
    where
        F: Fn(&Request<Body>) -> Response<Body> + Send + Sync + 'static,
    {
        let entry = RouteEntry {
            method: Some(method.to_string()),
            path_pattern: pattern.to_string(),
            handler: Arc::new(handler),
        };
        self.routes.write().unwrap().push(entry);
    }

    /// Quick method: return 401 + WWW-Authenticate header
    pub fn register_auth_challenge(&self, realm: &str, auth_type: &str) {
        let realm = realm.to_string();
        let auth_type = auth_type.to_string();
        self.on_get("/secret", move |_req: &Request<Body>| -> Response<Body> {
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(
                    "WWW-Authenticate",
                    format!("{} realm=\"{}\"", auth_type, realm),
                )
                .body(Body::from("Unauthorized"))
                .unwrap()
        });
    }

    /// Quick method: return gzip compressed response
    pub fn register_gzip_response(&self, path: &str, body: &[u8]) {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).expect("GZip encoding failed");
        let compressed = encoder.finish().expect("GZip finish failed");

        let path = path.to_string();
        self.on_get(
            path.as_str(),
            move |_req: &Request<Body>| -> Response<Body> {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Encoding", "gzip")
                    .header("Content-Type", "application/octet-stream")
                    .body(Body::from(compressed.clone()))
                    .unwrap()
            },
        );
    }

    /// Quick method: return chunked encoding response
    pub fn register_chunked_response(&self, path: &str, chunks: Vec<Vec<u8>>) {
        let path = path.to_string();
        self.on_get(
            path.as_str(),
            move |_req: &Request<Body>| -> Response<Body> {
                // Return plain data; hyper handles chunked encoding when
                // Transfer-Encoding header is set
                let body: Vec<u8> = chunks.iter().flatten().copied().collect();

                Response::builder()
                    .status(StatusCode::OK)
                    .header("Transfer-Encoding", "chunked")
                    .body(Body::from(body))
                    .unwrap()
            },
        );
    }

    /// Quick method: support Range requests (return 206 Partial Content)
    pub fn register_range_response(&self, path: &str, body: &[u8]) {
        let body = body.to_vec();
        let path = path.to_string();
        self.on_get(
            path.as_str(),
            move |req: &Request<Body>| -> Response<Body> {
                // Parse Range header
                if let Some(range_header) = req.headers().get("Range")
                    && let Ok(range_str) = range_header.to_str()
                {
                    // Parse "bytes=start-end"
                    if let Some(range_part) = range_str.strip_prefix("bytes=")
                        && let Some((start_str, end_str)) = range_part.split_once('-')
                    {
                        let start: u64 = start_str.parse().unwrap_or(0);
                        let end: u64 = end_str.parse().unwrap_or(body.len() as u64 - 1);
                        let end = end.min(body.len() as u64 - 1);

                        let slice = &body[start as usize..=end as usize];
                        return Response::builder()
                            .status(StatusCode::PARTIAL_CONTENT)
                            .header(
                                "Content-Range",
                                format!("bytes={}-{}/{}", start, end, body.len()),
                            )
                            .header("Accept-Ranges", "bytes")
                            .body(Body::from(slice.to_vec()))
                            .unwrap();
                    }
                }

                // Return full content without Range header
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", body.len())
                    .body(Body::from(body.clone()))
                    .unwrap()
            },
        );
    }

    /// Register a slow response handler that delays before responding (simulates slow server)
    ///
    /// # Arguments
    /// * `path` - URL path to match
    /// * `delay_ms` - Delay in milliseconds before sending response
    /// * `body` - Response body data
    pub fn register_slow_response(&self, path: &str, delay_ms: u64, body: &[u8]) {
        let path = path.to_string();
        let body = body.to_vec();
        self.on_get(
            path.as_str(),
            move |_req: &Request<Body>| -> Response<Body> {
                // Use async streaming Body to delay without blocking the tokio runtime.
                // hyper::Body::wrap_stream() accepts any impl Stream<Item = Result<Bytes, E>>.
                use futures::stream;
                let body_clone = body.clone();
                let stream = stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from(body_clone))
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Length", body.len())
                    .body(Body::wrap_stream(stream))
                    .unwrap()
            },
        );
    }

    /// Register a redirect chain handler (multi-hop redirects)
    ///
    /// # Arguments
    /// * `path` - Initial URL path
    /// * `statuses` - Sequence of HTTP status codes for each hop (e.g., [301, 302])
    /// * `final_body` - Response body returned at the end of the chain
    pub fn register_redirect_chain(&self, path: &str, statuses: &[u16], final_body: &[u8]) {
        let base_url_template = format!("http://127.0.0.1:{}", self.addr.port());
        let final_body = final_body.to_vec();

        if statuses.is_empty() {
            self.on_get(path, move |_| {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(final_body.clone()))
                    .unwrap()
            });
            return;
        }

        // Use a private namespace prefix (__rc__) to avoid route prefix-matching collisions.
        // Without this, "/chain" would also match "/chain-step1" via starts_with(),
        // causing the first route to intercept requests meant for intermediate steps.
        let ns = "__rc__";
        let base_id = format!("{}{}", ns, path.replace('/', "_"));

        // Register intermediate redirect steps and the final destination
        for (i, &status_code) in statuses.iter().enumerate() {
            let is_last = i == statuses.len() - 1;
            let step_path = if i == 0 {
                path.to_string()
            } else {
                format!("/{}-hop{}", base_id, i)
            };
            let next_path = if is_last {
                format!("/{}-final", base_id)
            } else {
                format!("/{}-hop{}", base_id, i + 1)
            };
            let location = format!("{}{}", base_url_template, next_path);
            let status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::FOUND);

            if is_last {
                // Final step returns actual content
                let fb = final_body.clone();
                let fp = step_path;
                self.on_get(fp.as_str(), move |_| {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Length", fb.len())
                        .body(Body::from(fb.clone()))
                        .unwrap()
                });
            } else {
                // Intermediate step redirects
                let sp = step_path;
                let loc = location.clone();
                self.on_get(sp.as_str(), move |_| {
                    Response::builder()
                        .status(status)
                        .header("Location", loc.as_str())
                        .header("Content-Length", "0")
                        .body(Body::empty())
                        .unwrap()
                });
            }
        }
    }

    /// Register an auth-gated resource (returns 401 unless valid Authorization header present)
    ///
    /// # Arguments
    /// * `path` - URL path to protect
    /// * `realm` - Authentication realm name
    /// * `auth_type` - Auth scheme type ("Basic" or "Digest")
    /// * `body` - Body data returned when auth succeeds
    pub fn register_auth_gated(&self, path: &str, realm: &str, auth_type: &str, body: &[u8]) {
        let realm = realm.to_string();
        let auth_type = auth_type.to_string();
        let body = body.to_vec();
        let path = path.to_string();

        self.on_get(
            path.as_str(),
            move |req: &Request<Body>| -> Response<Body> {
                // Validate Authorization header: must be present, non-empty,
                // AND start with the expected scheme prefix (e.g., "Basic " or "Digest ")
                let expected_prefix = format!("{} ", auth_type);
                let has_valid_auth = req
                    .headers()
                    .get("Authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|h| !h.is_empty() && h.starts_with(&expected_prefix))
                    .unwrap_or(false);

                if has_valid_auth {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Length", body.len())
                        .body(Body::from(body.clone()))
                        .unwrap()
                } else {
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .header(
                            "WWW-Authenticate",
                            format!("{} realm=\"{}\"", auth_type, realm),
                        )
                        .body(Body::from("Unauthorized"))
                        .unwrap()
                }
            },
        );
    }

    /// Register a partial-content server that only serves first N bytes then drops connection
    ///
    /// # Arguments
    /// * `path` - URL path
    /// * `full_body` - Full intended body (used for Content-Length header)
    /// * `serve_bytes` - Number of bytes to actually send before truncating
    pub fn register_partial_serve(&self, path: &str, full_body: &[u8], serve_bytes: usize) {
        let full_body = full_body.to_vec();
        let partial = full_body[..serve_bytes.min(full_body.len())].to_vec();
        let path = path.to_string();

        self.on_get(
            path.as_str(),
            move |_req: &Request<Body>| -> Response<Body> {
                // Claim full size but deliver truncated data (simulates connection drop)
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Length", full_body.len())
                    .body(Body::from(partial.clone()))
                    .unwrap()
            },
        );
    }

    /// Register a cookie echo-back verifier (sets cookie on first request, verifies on subsequent)
    ///
    /// First request to this path receives Set-Cookie header.
    /// Subsequent requests must include the Cookie header; otherwise returns 400.
    pub fn register_cookie_echo(&self, path: &str) {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let request_count = Arc::new(AtomicUsize::new(0));
        let seen_cookies: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let path = path.to_string();

        self.on_get(
            path.as_str(),
            move |req: &Request<Body>| -> Response<Body> {
                let count = request_count.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // First visit: set a session cookie
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("Set-Cookie", "session=test_abc123; Path=/")
                        .body(Body::from("cookie_set"))
                        .unwrap()
                } else {
                    // Subsequent visits: verify cookie is echoed back
                    let cookie_header = req
                        .headers()
                        .get("Cookie")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();

                    if cookie_header.contains("session=") {
                        seen_cookies.write().unwrap().insert(cookie_header);
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from("cookie_verified"))
                            .unwrap()
                    } else {
                        Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Body::from("missing_cookie"))
                            .unwrap()
                    }
                }
            },
        );

        // Store seen_cookies reference for test assertions (via a separate accessor pattern)
        // Note: In practice, tests verify via request_log instead of direct cookie set access
    }

    /// Get and clear request log (for assertion verification)
    pub fn take_request_log(&self) -> Vec<RequestLog> {
        let mut log = self.request_log.write().unwrap();
        std::mem::take(&mut *log)
    }

    /// Get server base URL
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// Shutdown server gracefully
    pub async fn shutdown(self) {
        if let Some(tx) = self.shutdown_tx
            && let Ok(sender) = Arc::try_unwrap(tx)
        {
            let _ = sender.send(());
        }
        // Give time for graceful shutdown
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}
