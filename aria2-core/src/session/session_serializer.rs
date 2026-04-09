use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadOptions, DownloadStatus, RequestGroup};

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub gid: u64,
    pub uris: Vec<String>,
    pub options: HashMap<String, String>,
    pub paused: bool,

    // New progress & status fields
    pub total_length: u64,
    pub completed_length: u64,
    pub upload_length: u64,
    pub download_speed: u64,
    pub status: String,           // "active"/"waiting"/"paused"/"error"
    pub error_code: Option<i32>,

    // BT-specific fields (only populated for BT downloads)
    pub bitfield: Option<Vec<u8>>,       // completed piece bitmap as hex string in file
    pub num_pieces: Option<u32>,
    pub piece_length: Option<u32>,
    pub info_hash_hex: Option<String>,   // for matching torrent files

    // HTTP/FTP resume info
    pub resume_offset: Option<u64>,      // already written file offset
}

impl SessionEntry {
    pub fn new(gid: u64, uris: Vec<String>) -> Self {
        SessionEntry {
            gid,
            uris,
            options: HashMap::new(),
            paused: false,

            // Default values for new fields
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            status: "active".to_string(),
            error_code: None,

            // BT-specific fields (None for non-BT downloads)
            bitfield: None,
            num_pieces: None,
            piece_length: None,
            info_hash_hex: None,

            // HTTP/FTP resume info
            resume_offset: None,
        }
    }

    pub fn with_options(mut self, options: HashMap<String, String>) -> Self {
        self.options = options;
        self
    }

    pub fn paused(mut self) -> Self {
        self.paused = true;
        self
    }

    fn get_option(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(|s| s.as_str())
    }
}

fn escape_uri(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape_uri(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    't' => {
                        result.push('\t');
                        chars.next();
                    }
                    'n' => {
                        result.push('\n');
                        chars.next();
                    }
                    '\\' => {
                        result.push('\\');
                        chars.next();
                    }
                    _ => {
                        result.push(c);
                    }
                }
            } else {
                result.push(c);
            }
        } else {
            result.push(c);
        }
    }
    result
}

pub fn serialize_entry(entry: &SessionEntry) -> String {
    let mut lines = String::new();

    let escaped_uris: Vec<String> = entry.uris.iter().map(|u| escape_uri(u)).collect();
    lines.push_str(&escaped_uris.join("\t"));
    lines.push('\n');

    lines.push_str(&format!(" GID={:x}\n", entry.gid));

    if entry.paused {
        lines.push_str(" PAUSE=true\n");
    }

    for (key, value) in &entry.options {
        lines.push_str(&format!(" {}={}\n", key, value));
    }

    // Serialize new progress & status fields
    lines.push_str(&format!(" TOTAL_LENGTH={}\n", entry.total_length));
    lines.push_str(&format!(" COMPLETED_LENGTH={}\n", entry.completed_length));
    lines.push_str(&format!(" UPLOAD_LENGTH={}\n", entry.upload_length));
    lines.push_str(&format!(" DOWNLOAD_SPEED={}\n", entry.download_speed));
    lines.push_str(&format!(" STATUS={}\n", entry.status));

    // ERROR_CODE (optional)
    match entry.error_code {
        Some(code) => lines.push_str(&format!(" ERROR_CODE={}\n", code)),
        None => lines.push_str(" ERROR_CODE=\n"),
    }

    // BITFIELD (hex encoded or empty)
    match &entry.bitfield {
        Some(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
            lines.push_str(&format!(" BITFIELD={}\n", hex));
        }
        None => lines.push_str(" BITFIELD=\n"),
    }

    // NUM_PIECES and PIECE_LENGTH
    lines.push_str(&format!(" NUM_PIECES={}\n",
        entry.num_pieces.unwrap_or(0)));
    lines.push_str(&format!(" PIECE_LENGTH={}\n",
        entry.piece_length.unwrap_or(0)));

    // INFO_HASH (optional)
    match &entry.info_hash_hex {
        Some(hash) => lines.push_str(&format!(" INFO_HASH={}\n", hash)),
        None => lines.push_str(" INFO_HASH=\n"),
    }

    // RESUME_OFFSET (optional)
    lines.push_str(&format!(" RESUME_OFFSET={}\n",
        entry.resume_offset.unwrap_or(0)));

    lines
}

fn download_options_to_map(opts: &DownloadOptions) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(v) = opts.split {
        map.insert("split".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_connection_per_server {
        map.insert("max-connection-per-server".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_download_limit {
        map.insert("max-download-limit".to_string(), v.to_string());
    }
    if let Some(v) = opts.max_upload_limit {
        map.insert("max-upload-limit".to_string(), v.to_string());
    }
    if let Some(ref v) = opts.dir {
        map.insert("dir".to_string(), v.clone());
    }
    if let Some(ref v) = opts.out {
        map.insert("out".to_string(), v.clone());
    }
    if let Some(v) = opts.seed_time {
        map.insert("seed-time".to_string(), v.to_string());
    }
    if let Some(v) = opts.seed_ratio {
        map.insert("seed-ratio".to_string(), v.to_string());
    }
    map
}

async fn group_to_entry(group: &RequestGroup) -> Option<SessionEntry> {
    let status = group.status().await;
    match status {
        DownloadStatus::Complete | DownloadStatus::Removed | DownloadStatus::Error(_) => None,
        _ => {
            let gid = group.gid().value();
            let uris = group.uris().to_vec();
            if uris.is_empty() {
                return None;
            }
            let options = download_options_to_map(group.options());
            let paused = matches!(status, DownloadStatus::Paused);

            // Extract progress information
            let total_length = group.total_length();
            let completed_length = group.completed_length().await;
            let upload_length = 0u64; // TODO: Track actual upload length if needed
            let download_speed = group.download_speed().await;

            // Convert DownloadStatus to string
            let status_str = match status {
                DownloadStatus::Active => "active",
                DownloadStatus::Waiting => "waiting",
                DownloadStatus::Paused => "paused",
                DownloadStatus::Complete | DownloadStatus::Removed => "complete",
                DownloadStatus::Error(_) => "error",
            }.to_string();

            // Extract error code if in error state
            let error_code = match &status {
                DownloadStatus::Error(_) => Some(1), // Generic error code
                _ => None,
            };

            Some(SessionEntry {
                gid,
                uris,
                options,
                paused,

                // Progress fields
                total_length,
                completed_length,
                upload_length,
                download_speed,
                status: status_str,
                error_code,

                // BT-specific fields (None for HTTP/FTP downloads)
                bitfield: None,
                num_pieces: None,
                piece_length: None,
                info_hash_hex: None,

                // Resume offset (use completed_length for now as a reasonable default)
                resume_offset: if completed_length > 0 { Some(completed_length) } else { None },
            })
        }
    }
}

pub async fn serialize_groups(groups: &[Arc<RwLock<RequestGroup>>]) -> Result<String> {
    let mut output = String::new();
    for group_lock in groups {
        let group = group_lock.read().await;
        if let Some(entry) = group_to_entry(&group).await {
            output.push_str(&serialize_entry(&entry));
            output.push('\n');
        }
    }
    Ok(output)
}

pub fn deserialize(text: &str) -> Result<Vec<SessionEntry>> {
    let mut entries = Vec::new();
    let mut current: Option<SessionEntry> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(ref mut entry) = current {
            if let Some(rest) = line.strip_prefix(' ') {
                let rest_trimmed = rest.trim();
                if let Some((key, value)) = rest_trimmed.split_once('=') {
                    let key = key.to_string();
                    let value = value.to_string();

                    // Handle known keys
                    match key.as_str() {
                        "GID" => {
                            if let Ok(gid) = u64::from_str_radix(&value, 16) {
                                entry.gid = gid;
                            }
                        }
                        "PAUSE" => {
                            if value == "true" {
                                entry.paused = true;
                            }
                        }
                        // New progress & status fields
                        "TOTAL_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.total_length = v;
                            }
                        }
                        "COMPLETED_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.completed_length = v;
                            }
                        }
                        "UPLOAD_LENGTH" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.upload_length = v;
                            }
                        }
                        "DOWNLOAD_SPEED" => {
                            if let Ok(v) = value.parse::<u64>() {
                                entry.download_speed = v;
                            }
                        }
                        "STATUS" => {
                            if !value.is_empty() {
                                entry.status = value;
                            }
                        }
                        "ERROR_CODE" => {
                            if !value.is_empty() {
                                if let Ok(code) = value.parse::<i32>() {
                                    entry.error_code = Some(code);
                                }
                            } else {
                                entry.error_code = None;
                            }
                        }
                        "BITFIELD" => {
                            if !value.is_empty() {
                                // Decode hex string back to Vec<u8>
                                if let Ok(bytes) = decode_hex(&value) {
                                    entry.bitfield = Some(bytes);
                                } else {
                                    tracing::warn!("Invalid BITFIELD hex string, ignoring");
                                    entry.bitfield = None;
                                }
                            } else {
                                entry.bitfield = None;
                            }
                        }
                        "NUM_PIECES" => {
                            if let Ok(v) = value.parse::<u32>() {
                                if v > 0 {
                                    entry.num_pieces = Some(v);
                                } else {
                                    entry.num_pieces = None;
                                }
                            }
                        }
                        "PIECE_LENGTH" => {
                            if let Ok(v) = value.parse::<u32>() {
                                if v > 0 {
                                    entry.piece_length = Some(v);
                                } else {
                                    entry.piece_length = None;
                                }
                            }
                        }
                        "INFO_HASH" => {
                            if !value.is_empty() {
                                entry.info_hash_hex = Some(value);
                            } else {
                                entry.info_hash_hex = None;
                            }
                        }
                        "RESUME_OFFSET" => {
                            if let Ok(v) = value.parse::<u64>() {
                                if v > 0 {
                                    entry.resume_offset = Some(v);
                                } else {
                                    entry.resume_offset = None;
                                }
                            }
                        }
                        _ => {
                            // Unknown key - store in options map (forward compatibility)
                            tracing::debug!("Unknown session key '{}', storing in options", key);
                            entry.options.insert(key, value);
                        }
                    }
                }
                continue;
            }
            entries.push(current.take().unwrap());
        }

        let unescaped = unescape_uri(line.trim());
        let uris: Vec<String> = unescaped
            .split('\t')
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !uris.is_empty() {
            current = Some(SessionEntry::new(0, uris));
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    Ok(entries)
}

/// Decode a hex string to Vec<u8>
fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(Aria2Error::Io(format!(
            "Hex string has odd length: {}",
            hex.len()
        )));
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte_str = &hex[i..i + 2];
        let byte = u8::from_str_radix(byte_str, 16).map_err(|e| {
            Aria2Error::Io(format!("Invalid hex character at position {}: {}", i, e))
        })?;
        bytes.push(byte);
    }

    Ok(bytes)
}

pub async fn load_from_file(path: &Path) -> Result<Vec<SessionEntry>> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Aria2Error::Io(format!("读取 session 文件失败 {}: {}", path.display(), e)))?;
    deserialize(&content)
}

pub async fn save_to_file(path: &Path, groups: &[Arc<RwLock<RequestGroup>>]) -> Result<()> {
    let content = serialize_groups(groups).await?;
    let tmp_path = path.with_extension("sess.tmp");
    tokio::fs::write(&tmp_path, &content).await.map_err(|e| {
        Aria2Error::Io(format!(
            "写入 session 临时文件失败 {}: {}",
            tmp_path.display(),
            e
        ))
    })?;
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| Aria2Error::Io(format!("重命名 session 文件失败 {}: {}", path.display(), e)))
}

/// 直接保存 SessionEntry 列表到文件（不经过 RequestGroup 转换）
///
/// 使用原子写入策略：先写入 .tmp 临时文件，然后重命名为目标文件
pub async fn save_to_file_with_entries(path: &Path, entries: &[SessionEntry]) -> Result<()> {
    let mut content = String::new();
    for entry in entries {
        content.push_str(&serialize_entry(entry));
        content.push('\n');
    }

    let tmp_path = path.with_extension("sess.tmp");
    tokio::fs::write(&tmp_path, &content).await.map_err(|e| {
        Aria2Error::Io(format!(
            "写入 session 临时文件失败 {}: {}",
            tmp_path.display(),
            e
        ))
    })?;
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| Aria2Error::Io(format!("重命名 session 文件失败 {}: {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_single_entry() {
        let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
        let text = serialize_entry(&entry);
        assert!(text.contains("http://example.com/file.zip"), "应包含 URI");
        assert!(text.contains("GID=d270c8a2"), "应包含 GID");
    }

    #[test]
    fn test_serialize_multiple_entries_roundtrip() {
        let entries = vec![
            SessionEntry::new(1, vec!["http://a.com/1.bin".to_string()]).with_options({
                let mut m = HashMap::new();
                m.insert("split".to_string(), "4".to_string());
                m.insert("dir".to_string(), "/tmp".to_string());
                m
            }),
            SessionEntry::new(
                2,
                vec![
                    "ftp://b.com/2.iso".to_string(),
                    "http://mirror.b.com/2.iso".to_string(),
                ],
            )
            .paused(),
        ];

        let mut serialized = String::new();
        for e in &entries {
            serialized.push_str(&serialize_entry(e));
            serialized.push('\n');
        }

        let deserialized = deserialize(&serialized).unwrap();
        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].uris.len(), 1);
        assert_eq!(deserialized[0].uris[0], "http://a.com/1.bin");
        assert_eq!(deserialized[0].options.get("split").unwrap(), "4");
        assert_eq!(deserialized[1].uris.len(), 2);
        assert!(deserialized[1].paused);
    }

    #[test]
    fn test_escape_unescape_uri() {
        assert_eq!(unescape_uri(&escape_uri("hello\tworld")), "hello\tworld");
        assert_eq!(unescape_uri(&escape_uri("line1\nline2")), "line1\nline2");
        assert_eq!(unescape_uri(&escape_uri("back\\slash")), "back\\slash");
        assert_eq!(unescape_uri(&escape_uri("normal")), "normal");
    }

    #[test]
    fn test_deserialize_empty_file() {
        let entries = deserialize("").unwrap();
        assert!(entries.is_empty());

        let entries = deserialize("\n\n\n").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_deserialize_skip_comments_and_blanks() {
        let input = r#"# This is a comment
# Another comment

http://example.com/file
 GID=abc123
 dir=/downloads

# Comment between entries
ftp://server/big.iso
 GID=def456
"#;
        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_deserialize_options_parsing() {
        let input = r#"http://example.com/file.zip
 GID=1
 split=4
 max-connection-per-server=2
 dir=C:\Users\test\Downloads
 out=file.zip
"#;
        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].options.get("split").unwrap(), "4");
        assert_eq!(
            entries[0].options.get("max-connection-per-server").unwrap(),
            "2"
        );
        assert_eq!(
            entries[0].options.get("dir").unwrap(),
            "C:\\Users\\test\\Downloads"
        );
        assert_eq!(entries[0].options.get("out").unwrap(), "file.zip");
    }

    #[test]
    fn test_pause_flag_serialization() {
        let input = r#"http://example.com/pause.me
 GID=42
 PAUSE=true
"#;
        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].paused);

        let text = serialize_entry(&entries[0]);
        assert!(text.contains("PAUSE=true"));
    }

    #[test]
    fn test_serialize_tab_separated_uris() {
        let entry = SessionEntry::new(
            99,
            vec![
                "http://mirror1.com/f".to_string(),
                "http://mirror2.com/f".to_string(),
                "http://mirror3.com/f".to_string(),
            ],
        );
        let text = serialize_entry(&entry);
        let uri_line = text.lines().next().unwrap();
        assert_eq!(uri_line.matches('\t').count(), 2, "3个URI应有2个tab分隔符");
    }

    #[tokio::test]
    async fn test_download_options_to_map_coverage() {
        let opts = DownloadOptions {
            split: Some(8),
            max_connection_per_server: Some(4),
            max_download_limit: Some(102400),
            max_upload_limit: Some(51200),
            dir: Some("/data".to_string()),
            out: Some("output.bin".to_string()),
            seed_time: Some(300),
            seed_ratio: Some(2.0),
            checksum: None,
            cookie_file: None,
            cookies: None,
            bt_force_encrypt: false,
            bt_require_crypto: false,
            enable_dht: true,
            dht_listen_port: Some(6881),
            enable_public_trackers: true,
            bt_piece_selection_strategy: "rarest-first".to_string(),
            bt_endgame_threshold: 20,
            max_retries: 3,
            retry_wait: 1,
            http_proxy: None,
            dht_file_path: None,
        };
        let map = download_options_to_map(&opts);
        assert_eq!(map.get("split").unwrap(), "8");
        assert_eq!(map.get("seed-ratio").unwrap(), "2");

        let empty_opts = DownloadOptions::default();
        let empty_map = download_options_to_map(&empty_opts);
        assert!(empty_map.is_empty());
    }

    // ==================== 新增测试用例 (Session 持久化增强) ====================

    #[test]
    fn test_serialize_new_fields() {
        let mut entry = SessionEntry::new(1, vec!["http://example.com/file.bin".to_string()]);
        entry.total_length = 1024 * 1024; // 1MB
        entry.completed_length = 512 * 1024; // 512KB
        entry.upload_length = 1024;
        entry.download_speed = 2048;
        entry.status = "active".to_string();
        entry.error_code = None;

        let text = serialize_entry(&entry);

        // 验证新字段出现在输出中
        assert!(text.contains("TOTAL_LENGTH=1048576"), "应包含 TOTAL_LENGTH");
        assert!(text.contains("COMPLETED_LENGTH=524288"), "应包含 COMPLETED_LENGTH");
        assert!(text.contains("UPLOAD_LENGTH=1024"), "应包含 UPLOAD_LENGTH");
        assert!(text.contains("DOWNLOAD_SPEED=2048"), "应包含 DOWNLOAD_SPEED");
        assert!(text.contains("STATUS=active"), "应包含 STATUS");
    }

    #[test]
    fn test_deserialize_with_all_fields() {
        let input = r#"http://example.com/bigfile.zip
 GID=1
 TOTAL_LENGTH=10485760
 COMPLETED_LENGTH=5242880
 UPLOAD_LENGTH=2048
 DOWNLOAD_SPEED=4096
 STATUS=active
 ERROR_CODE=
 BITFIELD=
 NUM_PIECES=0
 PIECE_LENGTH=0
 INFO_HASH=
 RESUME_OFFSET=5242880
"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);

        let entry = &entries[0];
        assert_eq!(entry.total_length, 10485760);
        assert_eq!(entry.completed_length, 5242880);
        assert_eq!(entry.upload_length, 2048);
        assert_eq!(entry.download_speed, 4096);
        assert_eq!(entry.status, "active");
        assert_eq!(entry.error_code, None);
        assert_eq!(entry.resume_offset, Some(5242880));
    }

    #[test]
    fn test_deserialize_backward_compat() {
        // 旧格式（没有新字段）应该能正常加载
        let input = r#"http://example.com/old-format.zip
 GID=abc123
 split=4
 dir=/downloads
"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);

        // 验证默认值
        assert_eq!(entries[0].total_length, 0, "旧格式应使用默认值 0");
        assert_eq!(entries[0].completed_length, 0, "旧格式应使用默认值 0");
        assert_eq!(entries[0].upload_length, 0, "旧格式应使用默认值 0");
        assert_eq!(entries[0].download_speed, 0, "旧格式应使用默认值 0");
        assert_eq!(entries[0].status, "active", "旧格式应使用默认状态 active");
        assert_eq!(entries[0].error_code, None, "旧格式应无错误代码");
        assert_eq!(entries[0].bitfield, None, "旧格式应无 bitfield");
        assert_eq!(entries[0].resume_offset, None, "旧格式应无 resume_offset");

        // 原有字段仍然正确
        assert_eq!(entries[0].options.get("split").unwrap(), "4");
    }

    #[test]
    fn test_deserialize_unknown_keys_ignored() {
        // 包含未知键的输入不应导致解析失败（向前兼容）
        let input = r#"http://example.com/file.zip
 GID=1
 UNKNOWN_KEY=some_value
 ANOTHER_UNKNOWN=42
 FUTURE_FIELD=data
 TOTAL_LENGTH=1000
"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);

        // 已知字段正常解析
        assert_eq!(entries[0].total_length, 1000);

        // 未知键被存储到 options 中
        assert_eq!(
            entries[0].options.get("UNKNOWN_KEY").unwrap(),
            "some_value"
        );
        assert_eq!(
            entries[0].options.get("ANOTHER_UNKNOWN").unwrap(),
            "42"
        );
        assert_eq!(
            entries[0].options.get("FUTURE_FIELD").unwrap(),
            "data"
        );
    }

    #[test]
    fn test_bitfield_roundtrip() {
        let mut entry = SessionEntry::new(1, vec!["http://example.com/torrent.torrent".to_string()]);
        // 设置 bitfield: [0xFF, 0xF0, 0x0F] - 表示某些 piece 已完成
        entry.bitfield = Some(vec![0xFF, 0xF0, 0x0F]);
        entry.num_pieces = Some(24); // 3 bytes * 8 bits = 24 pieces
        entry.piece_length = Some(262144); // 256KB

        let text = serialize_entry(&entry);

        // 验证 hex 编码
        assert!(text.contains("BITFIELD=fff00f"), "bitfield 应编码为 hex 字符串");

        // 反序列化验证
        let deserialized = deserialize(&text).unwrap();
        assert_eq!(deserialized.len(), 1);

        let restored = &deserialized[0];
        assert_eq!(restored.bitfield, Some(vec![0xFF, 0xF0, 0x0F]), "bitfield 应正确还原");
        assert_eq!(restored.num_pieces, Some(24));
        assert_eq!(restored.piece_length, Some(262144));
    }

    #[test]
    fn test_empty_bitfield_serialized_as_empty() {
        let entry = SessionEntry::new(1, vec!["http://example.com/file.zip".to_string()]);
        // bitfield 默认为 None

        let text = serialize_entry(&entry);

        // None bitfield 应该产生空值或空行
        assert!(text.contains("BITFIELD=\n"), "None bitfield 应序列化为空值");

        // 反序列化验证
        let deserialized = deserialize(&text).unwrap();
        assert_eq!(deserialized[0].bitfield, None, "空 bitfield 应还原为 None");
    }

    #[test]
    fn test_default_session_entry_has_zero_progress() {
        let entry = SessionEntry::new(99, vec!["http://test.com/f".to_string()]);

        // 验证所有新字段的默认值
        assert_eq!(entry.total_length, 0);
        assert_eq!(entry.completed_length, 0);
        assert_eq!(entry.upload_length, 0);
        assert_eq!(entry.download_speed, 0);
        assert_eq!(entry.status, "active", "默认状态应为 active");
        assert_eq!(entry.error_code, None);
        assert_eq!(entry.bitfield, None);
        assert_eq!(entry.num_pieces, None);
        assert_eq!(entry.piece_length, None);
        assert_eq!(entry.info_hash_hex, None);
        assert_eq!(entry.resume_offset, None);
    }

    #[test]
    fn test_status_field_values() {
        let statuses = ["active", "waiting", "paused", "error"];

        for status in statuses {
            let mut entry = SessionEntry::new(1, vec!["http://example.com/f".to_string()]);
            entry.status = status.to_string();

            let text = serialize_entry(&entry);
            assert!(
                text.contains(&format!("STATUS={}", status)),
                "状态 '{}' 应正确序列化",
                status
            );

            // 反序列化验证
            let deserialized = deserialize(&text).unwrap();
            assert_eq!(deserialized[0].status, status, "状态 '{}' 应正确反序列化", status);
        }
    }

    #[test]
    fn test_resume_offset_for_http_ftp() {
        let mut entry = SessionEntry::new(
            1,
            vec!["http://example.com/large-file.iso".to_string()],
        );
        // 模拟 HTTP 下载已写入部分数据
        entry.total_length = 1073741824; // 1GB
        entry.completed_length = 536870912; // 512MB 已完成
        entry.resume_offset = Some(536870912); // 从 512MB 处恢复
        entry.status = "paused".to_string();

        let text = serialize_entry(&entry);

        // 验证 resume offset 被正确序列化
        assert!(
            text.contains("RESUME_OFFSET=536870912"),
            "resume offset 应正确序列化"
        );

        // 反序列化并验证
        let deserialized = deserialize(&text).unwrap();
        assert_eq!(deserialized.len(), 1);
        assert_eq!(
            deserialized[0].resume_offset, Some(536870912),
            "resume offset 应正确还原"
        );
        assert_eq!(deserialized[0].status, "paused");
    }

    #[test]
    fn test_bt_specific_fields_only_when_present() {
        // 测试 BT 特定字段是可选的
        let mut entry = SessionEntry::new(
            1,
            vec!["magnet:?xt=urn:btih:abc123".to_string()],
        );

        // 不设置任何 BT 字段（保持 None）
        let text_without_bt = serialize_entry(&entry);
        let deserialized_without_bt = deserialize(&text_without_bt).unwrap();

        assert_eq!(deserialized_without_bt[0].bitfield, None);
        assert_eq!(deserialized_without_bt[0].num_pieces, None);
        assert_eq!(deserialized_without_bt[0].piece_length, None);
        assert_eq!(deserialized_without_bt[0].info_hash_hex, None);

        // 现在设置 BT 字段
        entry.bitfield = Some(vec![0xAA, 0xBB]);
        entry.num_pieces = Some(16);
        entry.piece_length = Some(524288);
        entry.info_hash_hex = Some("abc123def456".to_string());

        let text_with_bt = serialize_entry(&entry);
        let deserialized_with_bt = deserialize(&text_with_bt).unwrap();

        assert_eq!(deserialized_with_bt[0].bitfield, Some(vec![0xAA, 0xBB]));
        assert_eq!(deserialized_with_bt[0].num_pieces, Some(16));
        assert_eq!(deserialized_with_bt[0].piece_length, Some(524288));
        assert_eq!(
            deserialized_with_bt[0].info_hash_hex,
            Some("abc123def456".to_string())
        );
    }
}
