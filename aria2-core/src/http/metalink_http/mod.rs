//! Compatibility exports for the Metalink/HTTP protocol parser.

#[cfg(test)]
mod tests;

pub use aria2_protocol::http::metalink_http::{
    DEFAULT_PRI, MAX_PRI, MetalinkHttpDigest, MetalinkHttpLink, MetalinkHttpParser,
    MetalinkHttpResult,
};
