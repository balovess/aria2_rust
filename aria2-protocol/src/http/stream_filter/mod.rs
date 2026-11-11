//! Incremental codecs for HTTP message bodies.

mod bzip2;
mod chunked;
mod deflate;
mod gzip;
mod selection;
mod types;

pub use bzip2::BZip2Decoder;
pub use chunked::ChunkedDecoder;
pub use deflate::DeflateDecoder;
pub use gzip::GZipDecoder;
pub use selection::{HttpBodyCoding, detect_http_body_encoding, select_http_body_codings};
pub use types::{HttpStreamFilter, HttpStreamFilterError};
