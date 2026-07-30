// Helper utilities for mirror coordinator.

/// Extract host from URL.
///
/// Parses a URL string and returns the host portion (including port if present).
/// Returns the trimmed input as-is if no scheme separator (`://`) is found.
pub fn extract_host(url: &str) -> String {
    let url = url.trim();
    let scheme_pos = match url.find("://") {
        Some(pos) => pos,
        None => return url.to_string(),
    };
    let after_scheme = &url[scheme_pos + 3..];
    let host_part = if let Some(slash_idx) = after_scheme.find('/') {
        &after_scheme[..slash_idx]
    } else {
        after_scheme
    };
    host_part.to_string()
}
