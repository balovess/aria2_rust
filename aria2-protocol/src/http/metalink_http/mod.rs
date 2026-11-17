//! Parsers and values for the Metalink/HTTP response-header extensions.

mod helpers;
mod parser;
mod types;

#[cfg(test)]
mod tests;

pub use parser::MetalinkHttpParser;
pub use types::{DEFAULT_PRI, MAX_PRI, MetalinkHttpDigest, MetalinkHttpLink, MetalinkHttpResult};
