//! Compatibility access to HTTP cookie parsing primitives.

use std::time::{SystemTime, UNIX_EPOCH};

pub use aria2_protocol::http::cookie::parsing::format_http_date;
pub(crate) use aria2_protocol::http::cookie::parsing::{
    domain_matches, is_numeric_host, parse_http_date, path_matches,
};

/// Return the current time as Unix epoch seconds.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
