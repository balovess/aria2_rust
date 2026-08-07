pub mod config;
#[allow(clippy::module_inception)]
mod rate_limiter;
mod tests;
pub mod throttled_writer;
pub mod token_bucket;

// Re-export all public API items so that external crates can import from
// `crate::rate_limiter::RateLimiter` etc. without knowing the internal
// file layout.
pub use config::RateLimiterConfig;
pub use rate_limiter::RateLimiter;
pub use throttled_writer::ThrottledWriter;
pub use token_bucket::TokenBucket;
