//! HTTP request building and response parsing module (re-exports)
//!
//! This module re-exports types from the [`request`] and [`response`] submodules
//! for backward compatibility. New code should import directly from those modules.

// Re-export all request types
pub use crate::http::request::{
    HttpMethod, HttpRequest, HttpRequestBuilder, basic_auth, bearer_token,
};

// Re-export all response types
pub use crate::http::response::HttpResponse;
