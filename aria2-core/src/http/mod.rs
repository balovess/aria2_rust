pub mod client_pool;
pub mod conditional_get;
pub mod connection;
pub mod cookie;
pub mod cookie_storage;
pub mod digest_auth;
pub mod happy_eyeballs;
pub mod header_processor;
pub mod metalink_http;
pub mod ns_cookie_parser;
pub mod proxy;
pub mod proxy_tunnel;
pub mod request_response;
pub mod skip_response;
pub mod socks_connector;
pub mod splice_http;
pub mod stream_filter;

#[cfg(test)]
mod connection_tests;
#[cfg(test)]
mod stream_filter_tests;

// Re-export streaming header processor types
pub use header_processor::{HttpHeaderParseState, HttpHeaderProcessor, HttpResponseHead};

// Re-export Metalink/HTTP parser types for convenient access
pub use metalink_http::{DigestEntry, MetalinkHttpEntry, MetalinkHttpResult, parse_metalink_http};

// Re-export key types from proxy_tunnel for convenient access
pub use proxy_tunnel::{
    HttpProxyMode, HttpProxyTunnel, HttpProxyTunnelConfig, HttpProxyTunnelResult,
    HttpProxyRequestBuilder, ProxyAuthChallenge, ProxyAuthHandler, establish_http_proxy_tunnel,
};

// Re-export key types from proxy module for convenient access
pub use proxy::{HttpProxyConfig, HttpProxyForward, HttpProxyTunnel as HttpConnectProxyTunnel, ProxyResponse};

// Re-export key types from skip_response for convenient access
pub use skip_response::{
    AuthScheme, HttpAuthChallenge, HttpRedirectInfo, HttpSkipResponseHandler, RedirectType,
    SkipResponseResult, MAX_REDIRECT_COUNT,
};
