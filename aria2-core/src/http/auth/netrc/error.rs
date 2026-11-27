//! Netrc error types.

// ---------------------------------------------------------------------------
// NetrcError — parse / I/O errors
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing a `.netrc` file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetrcError {
    /// The file could not be found at the specified path.
    #[error("netrc file not found: {0}")]
    FileNotFound(String),
    /// An I/O error occurred while reading the file.
    #[error("netrc I/O error: {0}")]
    IoError(String),
    /// A parse error: unexpected token or premature EOF.
    #[error("netrc parse error: {0}")]
    ParseError(String),
}
