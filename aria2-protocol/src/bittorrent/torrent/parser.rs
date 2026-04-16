use std::collections::BTreeMap;
use tracing::{debug, info};

use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::torrent::info_hash::InfoHash;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub length: u64,
    pub path: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InfoDict {
    pub name: String,
    pub piece_length: u32,
    pub pieces: Vec<[u8; 20]>,
    pub length: Option<u64>,
    pub files: Option<Vec<FileEntry>>,
    pub private: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TorrentMeta {
    pub announce: String,
    pub announce_list: Vec<Vec<String>>,
    pub info: InfoDict,
    pub info_hash: InfoHash,
    pub creation_date: Option<i64>,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    pub encoding: Option<String>,
}

impl TorrentMeta {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        info!("开始解析torrent文件 ({} 字节)", data.len());
        let (root, _) =
            BencodeValue::decode(data).map_err(|e| format!("bencode解码失败: {}", e))?;

        let announce = root
            .dict_get_str("announce")
            .ok_or("缺少announce字段")?
            .to_string();

        let announce_list = Self::parse_announce_list(&root);

        let info = root.dict_get(b"info").ok_or("缺少info字典")?;

        let info_hash = InfoHash::from_info_value(info);
        debug!("info_hash: {}", info_hash.as_hex());

        let info_dict = Self::parse_info_dict(info)?;

        let creation_date = root.dict_get_int("creation date");
        let comment = root.dict_get_str("comment").map(|s| s.to_string());
        let created_by = root.dict_get_str("created by").map(|s| s.to_string());
        let encoding = root.dict_get_str("encoding").map(|s| s.to_string());

        let total_size = Self::compute_total_size(&info_dict);
        info!(
            "Torrent解析完成: name={}, pieces={}, size={}",
            info_dict.name,
            info_dict.pieces.len(),
            total_size
        );

        Ok(Self {
            announce,
            announce_list,
            info: info_dict,
            info_hash,
            creation_date,
            comment,
            created_by,
            encoding,
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

    fn parse_info_dict(info: &BencodeValue) -> Result<InfoDict, String> {
        let dict = info.as_dict().ok_or("info不是字典类型")?;

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
            .ok_or("无效或缺失的piece length")?;

        let pieces_raw = dict
            .get(&b"pieces"[..])
            .and_then(|v| v.as_bytes())
            .ok_or("缺失pieces字段")?;

        if pieces_raw.len() % 20 != 0 {
            return Err(format!("pieces长度({})不是20的倍数", pieces_raw.len()));
        }

        let pieces = (0..pieces_raw.len() / 20)
            .map(|i| {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(&pieces_raw[i * 20..(i + 1) * 20]);
                hash
            })
            .collect();

        let length = dict
            .get(&b"length"[..])
            .and_then(|v| v.as_int())
            .map(|n| n as u64);

        let files = if length.is_some() {
            None
        } else {
            Some(Self::parse_files(dict)?)
        };

        let private = dict.get(&b"private"[..]).and_then(|v| v.as_int());

        Ok(InfoDict {
            name,
            piece_length,
            pieces,
            length,
            files,
            private,
        })
    }

    fn parse_files(dict: &BTreeMap<Vec<u8>, BencodeValue>) -> Result<Vec<FileEntry>, String> {
        let files_val = dict
            .get(&b"files"[..])
            .and_then(|v| v.as_list())
            .ok_or("多文件模式缺少files字段")?;

        if files_val.is_empty() {
            return Err("files列表为空".to_string());
        }

        let mut entries = Vec::with_capacity(files_val.len());
        for file in files_val {
            let fd = file.as_dict().ok_or("file条目不是字典")?;
            let length = fd
                .get(&b"length"[..])
                .and_then(|v| v.as_int())
                .map(|n| n as u64)
                .ok_or("文件缺少length字段")?;
            let path_val = fd
                .get(&b"path"[..])
                .and_then(|v| v.as_list())
                .ok_or("文件缺少path字段")?;
            let path: Vec<String> = path_val
                .iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect();
            if path.is_empty() {
                return Err("文件path为空".to_string());
            }
            entries.push(FileEntry { length, path });
        }
        Ok(entries)
    }

    fn compute_total_size(info: &InfoDict) -> u64 {
        if let Some(len) = info.length {
            len
        } else if let Some(ref files) = info.files {
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
    }

    pub fn num_pieces(&self) -> usize {
        self.info.pieces.len()
    }

    pub fn total_size(&self) -> u64 {
        if let Some(len) = self.info.length {
            len
        } else if let Some(ref files) = self.info.files {
            files.iter().map(|f| f.length).sum()
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_info_hash_consistency() {
        let data = make_simple_torrent();
        let t1 = TorrentMeta::parse(&data).unwrap();
        let t2 = TorrentMeta::parse(&data).unwrap();
        assert_eq!(t1.info_hash.as_hex(), t2.info_hash.as_hex());
    }
}
