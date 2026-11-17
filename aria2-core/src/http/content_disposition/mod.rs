//! Compatibility exports for the HTTP Content-Disposition protocol parser.

#[cfg(test)]
mod tests;

pub use aria2_protocol::http::content_disposition::{
    ContentDispositionResult, extract_filename, parse_content_disposition,
    parse_content_disposition_with_default_utf8,
};
