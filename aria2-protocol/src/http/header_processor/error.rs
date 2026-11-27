/// Malformed or incomplete HTTP/1.x response-header input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HttpHeaderError(pub String);

pub type Result<T> = std::result::Result<T, HttpHeaderError>;
