use std::collections::BTreeMap;
use tracing::{debug, info};

use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::torrent::info_hash::InfoHash;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub length: u64,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2FileEntry {
    pub length: u64,
    pub path: Vec<String>,
    /// The root is mandatory for non-empty files and omitted for empty files.
    pub pieces_root: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
pub struct InfoDict {
    pub name: String,
    pub piece_length: u32,
    pub pieces: Vec<[u8; 20]>,
    pub length: Option<u64>,
    pub files: Option<Vec<FileEntry>>,
    pub private: Option<i64>,
    pub meta_version: Option<u64>,
    pub v2_files: Option<Vec<V2FileEntry>>,
    pub pieces_root: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
pub struct TorrentMeta {
    pub announce: String,
    pub announce_list: Vec<Vec<String>>,
    pub info: InfoDict,
    pub info_hash: InfoHash,
    pub info_hash_v2: Option<[u8; 32]>,
    pub piece_layers: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    pub creation_date: Option<i64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub encoding: Option<String>,
    /// Web seed URLs from url-list field (BEP 19)
    pub web_seeds: Vec<String>,
}

impl TorrentMeta {
    /// BEP 52 uses the first 20 bytes of the SHA-256 infohash on the wire.
    /// v1 torrents continue to use their SHA-1 infohash unchanged.
    pub fn network_info_hash(&self) -> [u8; 20] {
        if self.info.meta_version == Some(2) && self.info.pieces.is_empty() {
            self.info_hash_v2
                .map(|hash| hash[..20].try_into().expect("SHA-256 hash is 32 bytes"))
                .unwrap_or(self.info_hash.bytes)
        } else {
            // A hybrid metainfo has a v1 pieces field. It initially joins the
            // v1 swarm and may upgrade through the BEP 52 extension handshake.
            self.info_hash.bytes
        }
    }

    pub fn parse(data: &[u8]) -> Result<Self, String> {
        info!("Starting torrent file parsing ({} bytes)", data.len());
        let (root, _) =
            BencodeValue::decode(data).map_err(|e| format!("Bencode decoding failed: {}", e))?;

        let announce = root
            .dict_get_str("announce")
            .ok_or("Missing announce field")?
            .to_string();

        let announce_list = Self::parse_announce_list(&root);

        let info = root.dict_get(b"info").ok_or("Missing info dictionary")?;

        let info_hash = InfoHash::from_info_value(info);
        let meta_version = Self::parse_meta_version(info);
        if meta_version.is_some_and(|version| version > 2) {
            return Err(format!(
                "unsupported BitTorrent metainfo version {}",
                meta_version.unwrap_or_default()
            ));
        }
        let info_hash_v2 = (meta_version == Some(2)).then(|| InfoHash::from_info_value_v2(info));
        debug!("info_hash: {}", info_hash.as_hex());

        let info_dict = Self::parse_info_dict(info)?;
        if info_dict.meta_version == Some(2) && !info_dict.pieces.is_empty() {
            Self::validate_hybrid_layout(&info_dict)?;
        }
        let piece_layers = Self::parse_piece_layers(&root)?;
        if info_dict.meta_version == Some(2) {
            Self::validate_v2_piece_layers(&info_dict, &piece_layers)?;
        }

        let creation_date = root.dict_get_int("creation date");
        let comment = root.dict_get_str("comment").map(|s| s.to_string());
        let created_by = root.dict_get_str("created by").map(|s| s.to_string());
        let encoding = root.dict_get_str("encoding").map(|s| s.to_string());

        // Parse url-list (BEP 19 Web Seeds)
        let web_seeds = Self::parse_url_list(&root);

        let total_size = Self::compute_total_size(&info_dict);
        info!(
            "Torrent parsing complete: name={}, pieces={}, size={}, web_seeds={}",
            info_dict.name,
            info_dict.pieces.len(),
            total_size,
            web_seeds.len()
        );

        Ok(Self {
            announce,
            announce_list,
            info: info_dict,
            info_hash,
            info_hash_v2,
            piece_layers,
            creation_date,
            comment,
            created_by,
            encoding,
            web_seeds,
        })
    }

    fn parse_announce_list(root: &BencodeValue) -> Vec<Vec<String>> {
        match root.dict_get(b"announce-list") {
            Some(BencodeValue::List(tiers)) => tiers
                .iter()
                .filter_map(|tier| {
                    tier.as_list().map(|urls| {
                        urls.iter()
                            .filter_map(|u| u.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                })
                .filter(|t| !t.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Parse url-list field (BEP 19 Web Seeds)
    ///
    /// The url-list can be either:
    /// - A single string (one URL)
    /// - A list of strings (multiple fallback URLs)
    fn parse_url_list(root: &BencodeValue) -> Vec<String> {
        match root.dict_get(b"url-list") {
            Some(BencodeValue::Bytes(url_bytes)) => {
                // Single URL string
                std::str::from_utf8(url_bytes)
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default()
            }
            Some(BencodeValue::List(items)) => {
                // List of URL strings
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            }
            _ => Vec::new(), // Missing or wrong type
        }
    }

    fn parse_info_dict(info: &BencodeValue) -> Result<InfoDict, String> {
        let dict = info.as_dict().ok_or("info is not a dictionary type")?;

        let name = dict
            .get(&b"name"[..])
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unnamed".to_string());

        let piece_length = dict
            .get(&b"piece length"[..])
            .and_then(|v| v.as_int())
            .filter(|&n| n > 0 && n <= i32::MAX as i64)
            .map(|n| n as u32)
            .ok_or("Invalid or missing piece length")?;

        let meta_version = Self::parse_meta_version(info);
        if meta_version == Some(2)
            && (piece_length < crate::bittorrent::torrent::merkle::BLOCK_SIZE as u32
                || !piece_length.is_power_of_two())
        {
            return Err("BitTorrent v2 piece length must be a power of two >= 16 KiB".to_string());
        }
        let pieces = match dict.get(&b"pieces"[..]).and_then(|v| v.as_bytes()) {
            Some(pieces_raw) => {
                if pieces_raw.len() % 20 != 0 {
                    return Err(format!(
                        "pieces length ({}) is not a multiple of 20",
                        pieces_raw.len()
                    ));
                }
                (0..pieces_raw.len() / 20)
                    .map(|i| {
                        let mut hash = [0u8; 20];
                        hash.copy_from_slice(&pieces_raw[i * 20..(i + 1) * 20]);
                        hash
                    })
                    .collect()
            }
            None if meta_version == Some(2) => Vec::new(),
            None => return Err("Missing pieces field".to_string()),
        };

        let length = dict
            .get(&b"length"[..])
            .and_then(|v| v.as_int())
            .map(|n| u64::try_from(n).map_err(|_| "file length must be non-negative"))
            .transpose()?;

        let files = if length.is_some() {
            None
        } else if dict.contains_key(&b"files"[..]) {
            Some(Self::parse_files(dict)?)
        } else {
            None
        };

        let private = dict.get(&b"private"[..]).and_then(|v| v.as_int());
        let pieces_root = Self::parse_hash32(dict.get(&b"pieces root"[..]), "pieces root")?;
        let v2_files = if meta_version == Some(2) {
            Some(Self::parse_file_tree(
                dict.get(&b"file tree"[..])
                    .ok_or("BitTorrent v2 info dictionary missing file tree")?,
            )?)
        } else {
            None
        };

        Ok(InfoDict {
            name,
            piece_length,
            pieces,
            length,
            files,
            private,
            meta_version,
            v2_files,
            pieces_root,
        })
    }

    fn parse_files(dict: &BTreeMap<Vec<u8>, BencodeValue>) -> Result<Vec<FileEntry>, String> {
        let files_val = dict
            .get(&b"files"[..])
            .and_then(|v| v.as_list())
            .ok_or("Multi-file mode missing files field")?;

        if files_val.is_empty() {
            return Err("files list is empty".to_string());
        }

        let mut entries = Vec::with_capacity(files_val.len());
        for file in files_val {
            let fd = file.as_dict().ok_or("file entry is not a dictionary")?;
            let length = fd
                .get(&b"length"[..])
                .and_then(|v| v.as_int())
                .ok_or("file missing length field")
                .and_then(|n| u64::try_from(n).map_err(|_| "file length must be non-negative"))?;
            let path_val = fd
                .get(&b"path"[..])
                .and_then(|v| v.as_list())
                .ok_or("file missing path field")?;
            let path: Vec<String> = path_val
                .iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect();
            if path.is_empty() {
                return Err("file path is empty".to_string());
            }
            entries.push(FileEntry { length, path });
        }
        Ok(entries)
    }

    fn parse_meta_version(info: &BencodeValue) -> Option<u64> {
        info.as_dict()
            .and_then(|dict| dict.get(&b"meta version"[..]))
            .and_then(|value| value.as_int())
            .and_then(|value| u64::try_from(value).ok())
    }

    fn parse_hash32(value: Option<&BencodeValue>, field: &str) -> Result<Option<[u8; 32]>, String> {
        let Some(value) = value else { return Ok(None) };
        let bytes = value
            .as_bytes()
            .ok_or_else(|| format!("{field} must be a byte string"))?;
        bytes
            .try_into()
            .map(Some)
            .map_err(|_| format!("{field} must contain exactly 32 bytes"))
    }

    fn parse_file_tree(value: &BencodeValue) -> Result<Vec<V2FileEntry>, String> {
        fn walk(
            value: &BencodeValue,
            path: &mut Vec<String>,
            output: &mut Vec<V2FileEntry>,
        ) -> Result<(), String> {
            let dict = value
                .as_dict()
                .ok_or("file tree node must be a dictionary")?;
            for (name, child) in dict {
                let name = std::str::from_utf8(name)
                    .map_err(|_| "file tree path component is not valid UTF-8")?;
                if name.is_empty() {
                    if path.is_empty() {
                        return Err("file tree root cannot be a file".to_string());
                    }
                    if dict.len() != 1 {
                        return Err("file tree file node cannot have siblings".to_string());
                    }
                    let leaf = child
                        .as_dict()
                        .ok_or("file tree leaf must be a dictionary")?;
                    let length = leaf
                        .get(&b"length"[..])
                        .and_then(|value| value.as_int())
                        .and_then(|value| u64::try_from(value).ok())
                        .ok_or("file tree leaf missing valid length")?;
                    let pieces_root =
                        TorrentMeta::parse_hash32(leaf.get(&b"pieces root"[..]), "pieces root")?;
                    if length > 0 && pieces_root.is_none() {
                        return Err("non-empty file tree leaf missing pieces root".to_string());
                    }
                    output.push(V2FileEntry {
                        length,
                        path: path.clone(),
                        pieces_root,
                    });
                } else {
                    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
                        return Err("file tree contains an unsafe path component".to_string());
                    }
                    path.push(name.to_string());
                    walk(child, path, output)?;
                    path.pop();
                }
            }
            Ok(())
        }

        let mut output = Vec::new();
        walk(value, &mut Vec::new(), &mut output)?;
        if output.is_empty() {
            return Err("file tree contains no files".to_string());
        }
        Ok(output)
    }

    fn parse_piece_layers(
        root: &BencodeValue,
    ) -> Result<BTreeMap<[u8; 32], Vec<[u8; 32]>>, String> {
        let Some(value) = root.dict_get(b"piece layers") else {
            return Ok(BTreeMap::new());
        };
        let dict = value.as_dict().ok_or("piece layers must be a dictionary")?;
        let mut layers = BTreeMap::new();
        for (root_hash, layer) in dict {
            let root_hash: [u8; 32] = root_hash
                .as_slice()
                .try_into()
                .map_err(|_| "piece layer key must contain exactly 32 bytes")?;
            let layer = layer
                .as_bytes()
                .ok_or("piece layer must be a byte string")?;
            if layer.len() % 32 != 0 {
                return Err("piece layer length must be a multiple of 32 bytes".to_string());
            }
            let hashes = layer
                .as_chunks::<32>()
                .0
                .iter()
                .map(|chunk| {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(chunk);
                    hash
                })
                .collect();
            layers.insert(root_hash, hashes);
        }
        Ok(layers)
    }

    fn validate_v2_piece_layers(
        info: &InfoDict,
        layers: &BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    ) -> Result<(), String> {
        let piece_length = info.piece_length as u64;
        for file in info.v2_files.as_deref().unwrap_or_default() {
            let piece_count = file.length.div_ceil(piece_length) as usize;
            let Some(root) = file.pieces_root else {
                if file.length > 0 {
                    return Err("non-empty v2 file is missing its pieces root".to_string());
                }
                continue;
            };
            let Some(layer) = layers.get(&root) else {
                if file.length > piece_length {
                    return Err("v2 file is missing its piece layer".to_string());
                }
                continue;
            };
            if file.length <= piece_length {
                return Err(
                    "v2 piece layer is present for a file that does not need one".to_string(),
                );
            }
            if layer.len() != piece_count {
                return Err("v2 piece layer has the wrong number of hashes".to_string());
            }
            if !crate::bittorrent::torrent::merkle::verify_piece_layer(&root, layer) {
                return Err("v2 piece layer does not match pieces root".to_string());
            }
        }
        Ok(())
    }

    /// Validate the BEP 52 upgrade invariant for a hybrid metainfo. The v1
    /// piece stream must describe the same ordered content and address space;
    /// BEP 47 padding files account for v2's per-file piece alignment gaps.
    fn validate_hybrid_layout(info: &InfoDict) -> Result<(), String> {
        let Some(v2_files) = info.v2_files.as_deref() else {
            return Err("hybrid torrent is missing its v2 file tree".to_string());
        };
        let Some(v1_files) = info.files.as_deref() else {
            if let Some(length) = info.length {
                let v2_length = v2_files.iter().map(|file| file.length).sum::<u64>();
                if v2_files.len() != 1 || v2_length != length {
                    return Err("hybrid single-file layouts do not match".to_string());
                }
                return Ok(());
            }
            return Err("hybrid torrent is missing its v1 file list".to_string());
        };

        let piece_length = info.piece_length as u64;
        let mut v1_index = 0usize;
        let mut v2_offset = 0u64;
        for v2 in v2_files {
            let aligned_offset = if v2.length == 0 {
                v2_offset
            } else {
                v2_offset
                    .div_ceil(piece_length)
                    .checked_mul(piece_length)
                    .ok_or("hybrid v2 address space overflows")?
            };
            let expected_padding = aligned_offset - v2_offset;
            let mut actual_padding = 0u64;
            while v1_index < v1_files.len()
                && v1_files[v1_index]
                    .path
                    .first()
                    .is_some_and(|component| component == ".pad")
            {
                actual_padding = actual_padding
                    .checked_add(v1_files[v1_index].length)
                    .ok_or("hybrid v1 address space overflows")?;
                if actual_padding > expected_padding {
                    return Err("hybrid padding files do not match v2 alignment gaps".to_string());
                }
                v1_index += 1;
            }
            if actual_padding != expected_padding || v1_index >= v1_files.len() {
                return Err("hybrid padding files do not match v2 alignment gaps".to_string());
            }

            let v1 = &v1_files[v1_index];
            if v1.path != v2.path || v1.length != v2.length {
                return Err("hybrid v1/v2 file order or lengths do not match".to_string());
            }
            v1_index += 1;
            v2_offset = aligned_offset
                .checked_add(v2.length)
                .ok_or("hybrid v2 address space overflows")?;
        }
        if v1_index != v1_files.len() {
            return Err("hybrid v1/v2 file order or lengths do not match".to_string());
        }
        let v1_space = v1_files.iter().try_fold(0u64, |total, file| {
            total
                .checked_add(file.length)
                .ok_or("hybrid v1 address space overflows")
        })?;
        if v1_space != v2_offset {
            return Err("hybrid v1/v2 piece address spaces do not match".to_string());
        }
        if info.pieces.len() != v1_space.div_ceil(piece_length) as usize {
            return Err("hybrid v1 pieces do not cover the aligned address space".to_string());
        }
        Ok(())
    }

    fn compute_total_size(info: &InfoDict) -> u64 {
        if let Some(len) = info.length {
            len
        } else if let Some(ref files) = info.files {
            files
                .iter()
                .filter(|file| {
                    !file
                        .path
                        .first()
                        .is_some_and(|component| component == ".pad")
                })
                .map(|f| f.length)
                .sum()
        } else if let Some(ref files) = info.v2_files {
            files.iter().map(|f| f.length).sum()
        } else {
            0
        }
    }

    pub fn is_private(&self) -> bool {
        self.info.private.unwrap_or(0) != 0
    }

    pub fn is_single_file(&self) -> bool {
        self.info.length.is_some()
            || self
                .info
                .v2_files
                .as_ref()
                .is_some_and(|files| files.len() == 1)
    }

    pub fn num_pieces(&self) -> usize {
        if self.info.meta_version == Some(2) {
            self.piece_space_size()
                .div_ceil(self.info.piece_length as u64) as usize
        } else {
            self.info.pieces.len()
        }
    }

    /// BEP 52 address-space size, including the alignment gaps after files.
    pub fn piece_space_size(&self) -> u64 {
        if self.info.meta_version != Some(2) {
            return self.total_size();
        }
        let piece_length = self.info.piece_length as u64;
        self.info
            .v2_files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .fold(0u64, |offset, file| {
                if file.length == 0 {
                    offset
                } else {
                    offset.div_ceil(piece_length) * piece_length + file.length
                }
            })
    }

    pub fn total_size(&self) -> u64 {
        if let Some(len) = self.info.length {
            len
        } else if let Some(ref files) = self.info.files {
            files.iter().map(|f| f.length).sum()
        } else if let Some(ref files) = self.info.v2_files {
            files.iter().map(|f| f.length).sum()
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::Digest;
    use std::collections::BTreeMap;

    fn make_simple_torrent() -> Vec<u8> {
        let mut pieces_data = vec![0u8; 40];
        for (i, piece) in pieces_data.iter_mut().enumerate().take(40) {
            *piece = i as u8;
        }

        let mut info = BTreeMap::new();
        info.insert(
            b"name".to_vec(),
            BencodeValue::Bytes(b"test_file.bin".to_vec()),
        );
        info.insert(b"length".to_vec(), BencodeValue::Int(1024));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_data));

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        BencodeValue::Dict(root).encode()
    }

    #[test]
    fn test_parse_single_file_torrent() {
        let data = make_simple_torrent();
        let torrent = TorrentMeta::parse(&data).unwrap();

        assert_eq!(torrent.announce, "http://tracker.example.com/announce");
        assert_eq!(torrent.info.name, "test_file.bin");
        assert_eq!(torrent.info.piece_length, 512);
        assert_eq!(torrent.info.pieces.len(), 2);
        assert_eq!(torrent.info.length, Some(1024));
        assert!(torrent.is_single_file());
        assert!(!torrent.is_private());
        assert_eq!(torrent.num_pieces(), 2);
        assert_eq!(torrent.total_size(), 1024);
        assert_eq!(torrent.info.meta_version, None);
        assert!(torrent.info_hash_v2.is_none());
    }

    #[test]
    fn test_parse_v2_file_tree_and_piece_layers() {
        let piece_hash = [0x22u8; 32];
        let root_hash = crate::bittorrent::torrent::merkle::parent_hash(&piece_hash, &piece_hash);
        let mut leaf = BTreeMap::new();
        leaf.insert(b"length".to_vec(), BencodeValue::Int(32768));
        leaf.insert(
            b"pieces root".to_vec(),
            BencodeValue::Bytes(root_hash.to_vec()),
        );
        let mut file_name = BTreeMap::new();
        file_name.insert(b"".to_vec(), BencodeValue::Dict(leaf));
        let mut file_tree = BTreeMap::new();
        file_tree.insert(b"payload.bin".to_vec(), BencodeValue::Dict(file_name));

        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"payload".to_vec()));
        info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16384));
        info.insert(b"file tree".to_vec(), BencodeValue::Dict(file_tree));

        let mut layers = BTreeMap::new();
        layers.insert(
            root_hash.to_vec(),
            BencodeValue::Bytes(vec![piece_hash, piece_hash].into_iter().flatten().collect()),
        );
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));
        root.insert(b"piece layers".to_vec(), BencodeValue::Dict(layers));

        let torrent = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap();
        assert_eq!(torrent.info.meta_version, Some(2));
        assert_eq!(torrent.info.pieces.len(), 0);
        assert!(torrent.info_hash_v2.is_some());
        assert_eq!(
            torrent.network_info_hash(),
            torrent.info_hash_v2.unwrap()[..20]
        );
        let files = torrent.info.v2_files.as_ref().unwrap();
        assert_eq!(files[0].path, vec!["payload.bin"]);
        assert_eq!(files[0].pieces_root, Some(root_hash));
        assert_eq!(
            torrent.piece_layers.get(&root_hash),
            Some(&vec![piece_hash, piece_hash])
        );
        assert_eq!(torrent.piece_space_size(), 32768);
        assert_eq!(torrent.num_pieces(), 2);
    }

    #[test]
    fn test_v2_file_tree_rejects_invalid_piece_root() {
        let mut leaf = BTreeMap::new();
        leaf.insert(b"length".to_vec(), BencodeValue::Int(1));
        leaf.insert(b"pieces root".to_vec(), BencodeValue::Bytes(vec![0u8; 31]));
        let mut named = BTreeMap::new();
        named.insert(b"".to_vec(), BencodeValue::Dict(leaf));
        let mut tree = BTreeMap::new();
        tree.insert(b"file".to_vec(), BencodeValue::Dict(named));
        let mut info = BTreeMap::new();
        info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(1));
        info.insert(b"file tree".to_vec(), BencodeValue::Dict(tree));
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));
        assert!(TorrentMeta::parse(&BencodeValue::Dict(root).encode()).is_err());
    }

    #[test]
    fn test_v2_empty_file_may_omit_pieces_root() {
        let mut leaf = BTreeMap::new();
        leaf.insert(b"length".to_vec(), BencodeValue::Int(0));
        let mut named = BTreeMap::new();
        named.insert(b"".to_vec(), BencodeValue::Dict(leaf));
        let mut tree = BTreeMap::new();
        tree.insert(b"empty".to_vec(), BencodeValue::Dict(named));
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec()));
        info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16384));
        info.insert(b"file tree".to_vec(), BencodeValue::Dict(tree));
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let torrent = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap();
        assert_eq!(torrent.info.v2_files.unwrap()[0].pieces_root, None);
    }

    #[test]
    fn test_rejects_unsupported_metainfo_version() {
        let mut info = BTreeMap::new();
        info.insert(b"meta version".to_vec(), BencodeValue::Int(3));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16 * 1024));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
        let root = BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://x".to_vec()),
            ),
            (b"info".to_vec(), BencodeValue::Dict(info)),
        ]);
        let error = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap_err();
        assert!(error.contains("unsupported BitTorrent metainfo version 3"));
    }

    #[test]
    fn test_parse_multi_file_torrent() {
        let pieces_data = vec![0u8; 40];
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"multi_dir".to_vec()));

        let mut f1 = BTreeMap::new();
        f1.insert(b"length".to_vec(), BencodeValue::Int(500));
        f1.insert(
            b"path".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"dir1".to_vec()),
                BencodeValue::Bytes(b"file1.txt".to_vec()),
            ]),
        );

        let mut f2 = BTreeMap::new();
        f2.insert(b"length".to_vec(), BencodeValue::Int(524));
        f2.insert(
            b"path".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"dir2".to_vec()),
                BencodeValue::Bytes(b"file2.dat".to_vec()),
            ]),
        );

        info.insert(
            b"files".to_vec(),
            BencodeValue::List(vec![BencodeValue::Dict(f1), BencodeValue::Dict(f2)]),
        );
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_data));

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let data = BencodeValue::Dict(root).encode();
        let torrent = TorrentMeta::parse(&data).unwrap();

        assert!(!torrent.is_single_file());
        assert_eq!(torrent.info.files.as_ref().unwrap().len(), 2);
        assert_eq!(torrent.total_size(), 1024);
    }

    #[test]
    fn test_parse_with_optional_fields() {
        let _data = make_simple_torrent();

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(
            b"comment".to_vec(),
            BencodeValue::Bytes(b"A test torrent".to_vec()),
        );
        root.insert(
            b"created by".to_vec(),
            BencodeValue::Bytes(b"aria2-rust-tester".to_vec()),
        );
        root.insert(b"creation date".to_vec(), BencodeValue::Int(1700000000));

        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(100));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(50));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
        info.insert(b"private".to_vec(), BencodeValue::Int(1));

        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let data = BencodeValue::Dict(root).encode();
        let t = TorrentMeta::parse(&data).unwrap();
        assert_eq!(t.comment.as_deref(), Some("A test torrent"));
        assert_eq!(t.created_by.as_deref(), Some("aria2-rust-tester"));
        assert_eq!(t.creation_date, Some(1700000000));
        assert!(t.is_private());
    }

    #[test]
    fn test_error_missing_fields() {
        let empty = BencodeValue::Dict(BTreeMap::new()).encode();
        assert!(TorrentMeta::parse(&empty).is_err());

        let mut r = BTreeMap::new();
        r.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        );
        let no_info = BencodeValue::Dict(r).encode();
        assert!(TorrentMeta::parse(&no_info).is_err());
    }

    #[test]
    fn test_rejects_negative_single_file_length() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"file".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(-1));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16_384));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(Vec::new()));
        let root = BencodeValue::Dict(BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://x".to_vec()),
            ),
            (b"info".to_vec(), BencodeValue::Dict(info)),
        ]));

        let error = TorrentMeta::parse(&root.encode()).unwrap_err();
        assert!(error.contains("file length must be non-negative"));
    }

    #[test]
    fn test_rejects_negative_multi_file_length() {
        let file = BencodeValue::Dict(BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(-1)),
            (
                b"path".to_vec(),
                BencodeValue::List(vec![BencodeValue::Bytes(b"file".to_vec())]),
            ),
        ]));
        let info = BencodeValue::Dict(BTreeMap::from([
            (b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec())),
            (b"files".to_vec(), BencodeValue::List(vec![file])),
            (b"piece length".to_vec(), BencodeValue::Int(16_384)),
            (b"pieces".to_vec(), BencodeValue::Bytes(Vec::new())),
        ]));
        let root = BencodeValue::Dict(BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://x".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]));

        let error = TorrentMeta::parse(&root.encode()).unwrap_err();
        assert!(error.contains("file length must be non-negative"));
    }

    #[test]
    fn test_info_hash_consistency() {
        let data = make_simple_torrent();
        let t1 = TorrentMeta::parse(&data).unwrap();
        let t2 = TorrentMeta::parse(&data).unwrap();
        assert_eq!(t1.info_hash.as_hex(), t2.info_hash.as_hex());
    }

    #[test]
    fn test_parse_hybrid_single_file_validates_both_layouts() {
        let data = b"hybrid payload";
        let sha1_piece: [u8; 20] = sha1::Sha1::digest(data).into();
        let root = crate::bittorrent::torrent::merkle::file_root(data);
        let leaf = BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(data.len() as i64)),
            (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
        ]);
        let mut file_node = BTreeMap::new();
        file_node.insert(Vec::new(), BencodeValue::Dict(leaf));
        let mut file_tree = BTreeMap::new();
        file_tree.insert(b"hybrid.bin".to_vec(), BencodeValue::Dict(file_node));
        let info = BencodeValue::Dict(BTreeMap::from([
            (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
            (b"length".to_vec(), BencodeValue::Int(data.len() as i64)),
            (b"meta version".to_vec(), BencodeValue::Int(2)),
            (
                b"name".to_vec(),
                BencodeValue::Bytes(b"hybrid.bin".to_vec()),
            ),
            (b"piece length".to_vec(), BencodeValue::Int(16384)),
            (b"pieces".to_vec(), BencodeValue::Bytes(sha1_piece.to_vec())),
        ]));
        let root_dict = BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]);

        let torrent = TorrentMeta::parse(&BencodeValue::Dict(root_dict).encode()).unwrap();
        assert_eq!(torrent.info.meta_version, Some(2));
        assert_eq!(torrent.info.pieces.len(), 1);
        assert!(torrent.info_hash_v2.is_some());
    }

    #[test]
    fn test_parse_hybrid_rejects_mismatched_v1_length() {
        let root = crate::bittorrent::torrent::merkle::file_root(b"data");
        let leaf = BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(4)),
            (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
        ]);
        let mut file_node = BTreeMap::new();
        file_node.insert(Vec::new(), BencodeValue::Dict(leaf));
        let mut file_tree = BTreeMap::new();
        file_tree.insert(b"file.bin".to_vec(), BencodeValue::Dict(file_node));
        let info = BencodeValue::Dict(BTreeMap::from([
            (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
            (b"length".to_vec(), BencodeValue::Int(3)),
            (b"meta version".to_vec(), BencodeValue::Int(2)),
            (b"name".to_vec(), BencodeValue::Bytes(b"file.bin".to_vec())),
            (b"piece length".to_vec(), BencodeValue::Int(16384)),
            (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
        ]));
        let root_dict = BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]);
        let error = TorrentMeta::parse(&BencodeValue::Dict(root_dict).encode()).unwrap_err();
        assert!(error.contains("hybrid single-file layouts do not match"));
    }

    #[test]
    fn test_parse_hybrid_rejects_padding_at_wrong_position() {
        let piece_length = 16_384i64;
        let files = [
            (
                "one.bin",
                1i64,
                crate::bittorrent::torrent::merkle::file_root(b"1"),
            ),
            (
                "two.bin",
                1i64,
                crate::bittorrent::torrent::merkle::file_root(b"2"),
            ),
        ];
        let mut file_tree = BTreeMap::new();
        for (name, length, root) in files {
            file_tree.insert(
                name.as_bytes().to_vec(),
                BencodeValue::Dict(BTreeMap::from([(
                    Vec::new(),
                    BencodeValue::Dict(BTreeMap::from([
                        (b"length".to_vec(), BencodeValue::Int(length)),
                        (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
                    ])),
                )])),
            );
        }
        let content_file = |name: &[u8], length: i64| {
            BencodeValue::Dict(BTreeMap::from([
                (b"length".to_vec(), BencodeValue::Int(length)),
                (
                    b"path".to_vec(),
                    BencodeValue::List(vec![BencodeValue::Bytes(name.to_vec())]),
                ),
            ]))
        };
        let padding = BencodeValue::Dict(BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(16_383)),
            (
                b"path".to_vec(),
                BencodeValue::List(vec![
                    BencodeValue::Bytes(b".pad".to_vec()),
                    BencodeValue::Bytes(b"16383".to_vec()),
                ]),
            ),
        ]));
        let info = BencodeValue::Dict(BTreeMap::from([
            (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
            (b"meta version".to_vec(), BencodeValue::Int(2)),
            (b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec())),
            (b"piece length".to_vec(), BencodeValue::Int(piece_length)),
            (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 40])),
            (
                b"files".to_vec(),
                BencodeValue::List(vec![
                    content_file(b"one.bin", 1),
                    content_file(b"two.bin", 1),
                    padding,
                ]),
            ),
        ]));
        let root = BencodeValue::Dict(BTreeMap::from([
            (
                b"announce".to_vec(),
                BencodeValue::Bytes(b"http://x".to_vec()),
            ),
            (b"info".to_vec(), info),
        ]));

        let error = TorrentMeta::parse(&root.encode()).unwrap_err();
        assert!(error.contains("hybrid padding files do not match"));
    }

    #[test]
    fn test_parse_web_seeds_single() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(1024));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::Bytes(b"http://webseed.example.com/file.bin".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let data = BencodeValue::Dict(root).encode();
        let torrent = TorrentMeta::parse(&data).unwrap();

        assert_eq!(torrent.web_seeds.len(), 1);
        assert_eq!(torrent.web_seeds[0], "http://webseed.example.com/file.bin");
    }

    #[test]
    fn test_parse_web_seeds_multiple() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(2048));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 80]));

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"http://seed1.example.com/file.bin".to_vec()),
                BencodeValue::Bytes(b"http://seed2.example.com/file.bin".to_vec()),
                BencodeValue::Bytes(b"https://seed3.example.com/file.bin".to_vec()),
            ]),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let data = BencodeValue::Dict(root).encode();
        let torrent = TorrentMeta::parse(&data).unwrap();

        assert_eq!(torrent.web_seeds.len(), 3);
        assert_eq!(torrent.web_seeds[0], "http://seed1.example.com/file.bin");
        assert_eq!(torrent.web_seeds[1], "http://seed2.example.com/file.bin");
        assert_eq!(torrent.web_seeds[2], "https://seed3.example.com/file.bin");
    }

    #[test]
    fn test_parse_web_seeds_missing() {
        let data = make_simple_torrent();
        let torrent = TorrentMeta::parse(&data).unwrap();
        assert!(torrent.web_seeds.is_empty());
    }
}
