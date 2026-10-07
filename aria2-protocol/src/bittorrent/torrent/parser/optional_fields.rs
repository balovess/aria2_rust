use crate::bittorrent::bencode::codec::BencodeValue;

pub(super) fn parse_announce_list(root: &BencodeValue) -> Vec<Vec<String>> {
    match root.dict_get(b"announce-list") {
        Some(BencodeValue::List(tiers)) => tiers
            .iter()
            .filter_map(|tier| {
                tier.as_list().map(|urls| {
                    urls.iter()
                        .filter_map(|url| url.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
            })
            .filter(|tier| !tier.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse the BEP 19 `url-list`, accepting a single URL or a URL list.
pub(super) fn parse_url_list(root: &BencodeValue) -> Vec<String> {
    match root.dict_get(b"url-list") {
        Some(BencodeValue::Bytes(url_bytes)) => std::str::from_utf8(url_bytes)
            .map(|url| vec![url.to_owned()])
            .unwrap_or_default(),
        Some(BencodeValue::List(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse optional DHT bootstrap nodes from the top-level `nodes` field.
pub(super) fn parse_nodes(root: &BencodeValue) -> Vec<(String, u16)> {
    root.dict_get(b"nodes")
        .and_then(BencodeValue::as_list)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let pair = entry.as_list()?;
            if pair.len() != 2 {
                return None;
            }
            let host = pair[0].as_str()?.trim();
            let port = pair[1].as_int()?.try_into().ok()?;
            if port == 0 {
                return None;
            }
            (!host.is_empty()).then(|| (host.to_owned(), port))
        })
        .collect()
}
