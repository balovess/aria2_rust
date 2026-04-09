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
}

impl SessionEntry {
    pub fn new(gid: u64, uris: Vec<String>) -> Self {
        SessionEntry {
            gid,
            uris,
            options: HashMap::new(),
            paused: false,
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
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape_uri(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                match next {
                    't' => { result.push('\t'); chars.next(); }
                    'n' => { result.push('\n'); chars.next(); }
                    '\\' => { result.push('\\'); chars.next(); }
                    _ => { result.push(c); }
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

            Some(SessionEntry {
                gid,
                uris,
                options,
                paused,
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
                    if key == "GID" {
                        if let Ok(gid) = u64::from_str_radix(&value, 16) {
                            entry.gid = gid;
                        }
                    } else if key == "PAUSE" && value == "true" {
                        entry.paused = true;
                    } else {
                        entry.options.insert(key, value);
                    }
                }
                continue;
            }
            entries.push(current.take().unwrap());
        }

        let unescaped = unescape_uri(line.trim());
        let uris: Vec<String> = unescaped.split('\t').map(|s| s.to_string()).filter(|s| !s.is_empty()).collect();
        if !uris.is_empty() {
            current = Some(SessionEntry::new(0, uris));
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    Ok(entries)
}

pub async fn load_from_file(path: &Path) -> Result<Vec<SessionEntry>> {
    let content = tokio::fs::read_to_string(path).await
        .map_err(|e| Aria2Error::Io(format!("读取 session 文件失败 {}: {}", path.display(), e)))?;
    deserialize(&content)
}

pub async fn save_to_file(path: &Path, groups: &[Arc<RwLock<RequestGroup>>]) -> Result<()> {
    let content = serialize_groups(groups).await?;
    let tmp_path = path.with_extension("sess.tmp");
    tokio::fs::write(&tmp_path, &content).await
        .map_err(|e| Aria2Error::Io(format!("写入 session 临时文件失败 {}: {}", tmp_path.display(), e)))?;
    tokio::fs::rename(&tmp_path, path).await
        .map_err(|e| Aria2Error::Io(format!("重命名 session 文件失败 {}: {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_single_entry() {
        let entry = SessionEntry::new(0xd270c8a2, vec![
            "http://example.com/file.zip".to_string(),
        ]);
        let text = serialize_entry(&entry);
        assert!(text.contains("http://example.com/file.zip"), "应包含 URI");
        assert!(text.contains("GID=d270c8a2"), "应包含 GID");
    }

    #[test]
    fn test_serialize_multiple_entries_roundtrip() {
        let entries = vec![
            SessionEntry::new(1, vec!["http://a.com/1.bin".to_string()])
                .with_options({
                    let mut m = HashMap::new();
                    m.insert("split".to_string(), "4".to_string());
                    m.insert("dir".to_string(), "/tmp".to_string());
                    m
                }),
            SessionEntry::new(2, vec!["ftp://b.com/2.iso".to_string(), "http://mirror.b.com/2.iso".to_string()])
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
        assert_eq!(entries[0].options.get("max-connection-per-server").unwrap(), "2");
        assert_eq!(entries[0].options.get("dir").unwrap(), "C:\\Users\\test\\Downloads");
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
        let entry = SessionEntry::new(99, vec![
            "http://mirror1.com/f".to_string(),
            "http://mirror2.com/f".to_string(),
            "http://mirror3.com/f".to_string(),
        ]);
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
        };
        let map = download_options_to_map(&opts);
        assert_eq!(map.get("split").unwrap(), "8");
        assert_eq!(map.get("seed-ratio").unwrap(), "2");

        let empty_opts = DownloadOptions::default();
        let empty_map = download_options_to_map(&empty_opts);
        assert!(empty_map.is_empty());
    }
}
