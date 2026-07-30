//! Netrc error types.

use std::fmt;

// ---------------------------------------------------------------------------
// NetrcError — parse / I/O errors
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing a `.netrc` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetrcError {
    /// The file could not be found at the specified path.
    FileNotFound(String),
    /// An I/O error occurred while reading the file.
    IoError(String),
    /// A parse error: unexpected token or premature EOF.
    ParseError(String),
}

impl fmt::Display for NetrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotFound(p) => write!(f, "netrc file not found: {}", p),
            Self::IoError(e) => write!(f, "netrc I/O error: {}", e),
            Self::ParseError(e) => write!(f, "netrc parse error: {}", e),
        }
    }
}

impl std::error::Error for NetrcError {}
