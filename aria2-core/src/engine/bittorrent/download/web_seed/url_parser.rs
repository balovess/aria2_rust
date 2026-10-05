//! Parsing of `url-list` from torrent metadata (BEP 19 / BEP 17).

/// Parse the `url-list` field from torrent metadata.
///
/// The BEP 19 / RFC 6986 `url-list` key can be either:
/// - A single string (one URL)
/// - A list of strings (multiple fallback URLs)
///
/// Returns an empty vector if the key is missing or malformed.
///
/// # Arguments
///
/// * `meta` - Parsed torrent metadata structure
///
/// # Returns
///
/// * `Vec<String>` - List of extracted web-seed URLs
///
/// # Example
///
/// ```ignore
/// let urls = parse_url_list(&torrent_meta);
/// if !urls.is_empty() {
///     let manager = WebSeedManager::new(urls, piece_length, total_length);
/// }
/// ```
pub fn parse_url_list(
    meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
) -> Vec<String> {
    meta.web_seeds.clone()
}

/// Parse url-list directly from raw bencoded torrent data.
///
/// This is the working implementation that decodes the raw torrent bytes
/// and extracts the `url-list` key at the top level of the bencode dictionary.
///
/// # Arguments
///
/// * `torrent_bytes` - Raw bencoded torrent file contents
///
/// # Returns
///
/// * `Vec<String>` - Extracted web-seed URLs (empty if missing/malformed)
pub fn parse_url_list_from_bytes(torrent_bytes: &[u8]) -> Vec<String> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let (root, _) = match BencodeValue::decode(torrent_bytes) {
        Ok(result) => result,
        Err(_) => return Vec::new(),
    };

    match root.dict_get(b"url-list") {
        Some(BencodeValue::Bytes(url_bytes)) => {
            // Single URL string
            match std::str::from_utf8(url_bytes) {
                Ok(url) => vec![url.to_string()],
                Err(_) => Vec::new(),
            }
        }
        Some(BencodeValue::List(items)) => {
            // List of URL strings
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|s| s.to_string())
                .collect()
        }
        _ => Vec::new(), // Missing or wrong type
    }
}
