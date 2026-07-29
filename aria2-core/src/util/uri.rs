//! URI parsing, construction, and resolution utilities.
//!
//! Port of the C++ `uri.h` / `uri.cc` module from aria2. Provides:
//! - `UriStruct`: parsed URI components (protocol, host, dir, file, query, etc.)
//! - `parse()`: split a URI string into `UriStruct`
//! - `construct()`: rebuild a URI string from `UriStruct`
//! - `normalize_path()`: resolve `.` / `..` and collapse duplicate `/`
//! - `join_path()`: combine base + relative path with normalization
//! - `join_uri()`: resolve a (possibly relative) URI against a base URI

mod construct;
mod join;
mod normalize;
mod parse;
mod percent;
mod structs;

#[cfg(test)]
mod tests;

// Re-export all public items so that external code using
// `crate::util::uri::X` still works.
pub use construct::construct;
pub use join::join_uri;
pub use normalize::{join_path, normalize_path};
pub use parse::parse;
pub use percent::{percent_decode, percent_encode};
pub use structs::{UriStruct, get_default_port};
