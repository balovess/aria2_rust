//! BitTorrent metainfo transformations used by magnet downloads.

use std::collections::BTreeMap;

use crate::bittorrent::bencode::codec::BencodeValue;

/// Wrap a BEP 9 `info` dictionary in a torrent root with magnet trackers.
///
/// Complete torrent documents are returned unchanged so exact-source and
/// cached metadata remain byte-for-byte stable.
pub fn normalize_magnet_metadata(metadata: &[u8], trackers: &[String]) -> Result<Vec<u8>, String> {
    let (value, consumed) = BencodeValue::decode(metadata)?;
    if consumed != metadata.len() {
        return Err("BEP 9 metadata contains trailing bytes".to_string());
    }

    let BencodeValue::Dict(info_dict) = value else {
        return Err("BEP 9 metadata is not a dictionary".to_string());
    };

    if info_dict
        .get(b"info".as_slice())
        .is_some_and(|value| matches!(value, BencodeValue::Dict(_)))
    {
        return Ok(metadata.to_vec());
    }

    let announce = trackers
        .first()
        .map_or_else(Vec::new, |url| url.as_bytes().to_vec());
    let announce_list = (!trackers.is_empty()).then(|| {
        BencodeValue::List(
            trackers
                .iter()
                .map(|url| BencodeValue::List(vec![BencodeValue::Bytes(url.as_bytes().to_vec())]))
                .collect(),
        )
    });

    let mut root = BTreeMap::new();
    root.insert(b"announce".to_vec(), BencodeValue::Bytes(announce));
    if let Some(announce_list) = announce_list {
        root.insert(b"announce-list".to_vec(), announce_list);
    }
    root.insert(b"info".to_vec(), BencodeValue::Dict(info_dict));
    Ok(BencodeValue::Dict(root).encode())
}

/// Add magnet `ws` values to the root `url-list`, preserving existing seeds.
pub fn merge_magnet_web_seeds(
    torrent_bytes: &[u8],
    additional_web_seeds: &[String],
) -> Result<Vec<u8>, String> {
    if additional_web_seeds.is_empty() {
        return Ok(torrent_bytes.to_vec());
    }

    let (mut root, consumed) = BencodeValue::decode(torrent_bytes)?;
    if consumed != torrent_bytes.len() {
        return Err("Torrent metadata contains trailing bytes".to_string());
    }
    let dict = match &mut root {
        BencodeValue::Dict(dict) => dict,
        _ => return Err("Torrent metadata root is not a dictionary".to_string()),
    };

    let mut web_seeds = Vec::new();
    if let Some(existing) = dict.remove(b"url-list".as_slice()) {
        match existing {
            BencodeValue::Bytes(url) => web_seeds.push(url),
            BencodeValue::List(urls) => {
                web_seeds.extend(urls.into_iter().filter_map(|url| match url {
                    BencodeValue::Bytes(url) => Some(url),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    for url in additional_web_seeds {
        let url = url.as_bytes().to_vec();
        if !web_seeds.iter().any(|existing| existing == &url) {
            web_seeds.push(url);
        }
    }

    let url_list = if web_seeds.len() == 1 {
        BencodeValue::Bytes(web_seeds.pop().expect("one web seed is present"))
    } else {
        BencodeValue::List(web_seeds.into_iter().map(BencodeValue::Bytes).collect())
    };
    dict.insert(b"url-list".to_vec(), url_list);
    Ok(root.encode())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_info() -> Vec<u8> {
        let mut info = BTreeMap::new();
        info.insert(b"length".to_vec(), BencodeValue::Int(1));
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"file.bin".to_vec()));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(1));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
        BencodeValue::Dict(info).encode()
    }

    #[test]
    fn wraps_info_dictionary_with_trackers_and_keeps_complete_metainfo() {
        let info = encoded_info();
        let trackers = vec!["https://tracker.test/announce".to_string()];
        let normalized = normalize_magnet_metadata(&info, &trackers).unwrap();
        let (root, consumed) = BencodeValue::decode(&normalized).unwrap();
        assert_eq!(consumed, normalized.len());
        assert_eq!(
            root.dict_get(b"announce").unwrap().as_bytes(),
            Some(b"https://tracker.test/announce".as_slice())
        );

        assert_eq!(
            normalize_magnet_metadata(&normalized, &trackers).unwrap(),
            normalized
        );
    }

    #[test]
    fn merges_web_seeds_without_duplicates_and_preserves_info_bytes() {
        let info = encoded_info();
        let root = BencodeValue::Dict(BTreeMap::from([
            (b"info".to_vec(), BencodeValue::decode(&info).unwrap().0),
            (
                b"url-list".to_vec(),
                BencodeValue::Bytes(b"https://seed.test/a".to_vec()),
            ),
        ]));
        let torrent = root.encode();
        let merged = merge_magnet_web_seeds(
            &torrent,
            &[
                "https://seed.test/a".to_string(),
                "https://seed.test/b".to_string(),
            ],
        )
        .unwrap();
        let (root, consumed) = BencodeValue::decode(&merged).unwrap();
        assert_eq!(consumed, merged.len());
        assert_eq!(root.dict_get(b"info").unwrap().encode(), info);
        assert_eq!(
            root.dict_get(b"url-list").unwrap(),
            &BencodeValue::List(vec![
                BencodeValue::Bytes(b"https://seed.test/a".to_vec()),
                BencodeValue::Bytes(b"https://seed.test/b".to_vec()),
            ])
        );
    }

    #[test]
    fn rejects_trailing_data_and_non_dictionary_metadata() {
        assert!(normalize_magnet_metadata(&[b"i1e".as_slice(), b"junk"].concat(), &[]).is_err());
        assert!(normalize_magnet_metadata(b"i1e", &[]).is_err());
        assert!(merge_magnet_web_seeds(b"d1:ai1eejunk", &["https://seed.test".into()]).is_err());
    }
}
