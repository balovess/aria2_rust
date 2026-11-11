use thiserror::Error;

/// Errors produced while decoding an HTTP message body.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HttpStreamFilterError {
    #[error("HTTP body parse error: {0}")]
    Parse(String),
    #[error("HTTP body decompression error: {0}")]
    Io(String),
    #[error("HTTP protocol error: {0}")]
    Protocol(String),
}

/// Stateful incremental decoder for one HTTP body coding.
pub trait HttpStreamFilter: Send + Sync + std::fmt::Debug {
    fn filter(&mut self, input: &[u8]) -> Result<Vec<u8>, HttpStreamFilterError>;
    fn flush(&mut self) -> Result<Vec<u8>, HttpStreamFilterError>;
    fn name(&self) -> &'static str;
    fn needs_more_input(&self) -> bool;
}
