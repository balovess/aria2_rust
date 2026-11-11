use std::fmt;

/// Malformed or incomplete HTTP/1.x response-header input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpHeaderError(pub String);

impl fmt::Display for HttpHeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HttpHeaderError {}

pub type Result<T> = std::result::Result<T, HttpHeaderError>;
