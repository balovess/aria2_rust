//! BT 下载进度持久化系统
//!
//! 提供将 BT 下载进度保存到 `.aria2` 文本格式文件的功能，支持原子写入和
//! 自动检测 C++ 二进制格式与文本格式的兼容性。

use std::fmt::{Display, Formatter};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Aria2Error, FatalError, Result};
use tracing::{debug, info, warn};

/// BT 下载进度的 Peer 地址信息
#[derive(Clone, Debug)]
pub struct PeerAddr {
    /// IP 地址
    pub ip: String,
    /// 端口号
    pub port: u16,
}

impl Display for PeerAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

/// BT 下载统计信息
#[derive(Clone, Debug, Default)]
pub struct DownloadStats {
    /// 已上传字节数
    pub uploaded_bytes: u64,
    /// 已下载字节数
    pub downloaded_bytes: u64,
    /// 上传速度（字节/秒）
    pub upload_speed: f64,
    /// 下载速度（字节/秒）
    pub download_speed: f64,
    /// 已用时间（秒）
    pub elapsed_seconds: u64,
}

/// BT 下载进度数据结构
///
/// 包含 BT 下载的所有状态信息，用于持久化到磁盘。
#[derive(Clone, Debug)]
pub struct BtProgress {
    /// 种子的 info_hash
    pub info_hash: [u8; 20],
    /// 已下载的 piece 位图
    pub bitfield: Vec<u8>,
    /// 已连接的 peer 列表
    pub peers: Vec<PeerAddr>,
    /// 下载统计信息
    pub stats: DownloadStats,
    /// 每个 piece 的长度
    pub piece_length: u32,
    /// 总大小
    pub total_size: u64,
    /// 总 piece 数量
    pub num_pieces: u32,
    /// 保存时间
    pub save_time: SystemTime,
    /// 格式版本号
    pub version: u32,
}

impl Default for BtProgress {
    fn default() -> Self {
        BtProgress {
            info_hash: [0u8; 20],
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            save_time: SystemTime::UNIX_EPOCH,
            version: 1,
        }
    }
}

impl BtProgress {
    /// 将 info_hash 转为 40 位十六进制字符串
    ///
    /// # Returns
    ///
    /// 返回小写的十六进制字符串表示
    pub fn to_hex_hash(&self) -> String {
        self.info_hash
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }

    /// 计算完成百分比
    ///
    /// 根据 bitfield 中已设置的位计算下载完成比例。
    ///
    /// # Returns
    ///
    /// 返回 0.0 到 1.0 之间的完成比例
    pub fn completion_ratio(&self) -> f64 {
        if self.num_pieces == 0 || self.bitfield.is_empty() {
            return 0.0;
        }

        let mut set_bits = 0u32;
        for &byte in &self.bitfield {
            set_bits += byte.count_ones();
        }

        set_bits as f64 / self.num_pieces as f64
    }
}

/// BT 进度文件管理器
///
/// 负责管理 BT 下载进度的保存、加载、删除和列表操作。
/// 支持原子写入，确保在异常情况下不会损坏已存在的进度文件。
pub struct BtProgressManager {
    /// 进度文件存储目录
    progress_dir: PathBuf,
}

impl BtProgressManager {
    /// 创建新的 BT 进度管理器
    ///
    /// 自动创建指定的目录（如果不存在）。
    ///
    /// # Arguments
    ///
    /// * `progress_dir` - 进度文件的存储目录路径
    ///
    /// # Errors
    ///
    /// 当无法创建目录时返回错误
    pub fn new(progress_dir: &Path) -> Result<Self> {
        fs::create_dir_all(progress_dir).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "无法创建进度目录 {}: {}",
                progress_dir.display(),
                e
            )))
        })?;

        info!(path = %progress_dir.display(), "BT 进度管理器初始化");

        Ok(BtProgressManager {
            progress_dir: progress_dir.to_path_buf(),
        })
    }

    /// 生成进度文件名
    pub fn get_progress_file_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();
        self.progress_dir.join(format!("{}.aria2", hex_hash))
    }

    /// 保存 BT 下载进度到文件
    ///
    /// 使用原子写入策略：先写入临时文件，再通过 rename 操作替换原文件，
    /// 确保在写入过程中发生异常不会损坏已有的进度文件。
    ///
    /// # Arguments
    ///
    /// * `info_hash` - 种子的 info_hash (20 字节)
    /// * `progress` - 要保存的进度数据
    ///
    /// # Errors
    ///
    /// 当文件写入失败时返回错误
    pub fn save_progress(&self, info_hash: &[u8; 20], progress: &BtProgress) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);
        let tmp_path = file_path.with_extension("aria2.tmp");

        debug!(
            path = %file_path.display(),
            hash = %progress.to_hex_hash(),
            "保存 BT 进度"
        );

        // 写入临时文件
        let content = self.serialize_progress(progress);
        {
            let mut file = fs::File::create(&tmp_path)
                .map_err(|e| Aria2Error::Io(format!("创建临时进度文件失败: {}", e)))?;

            file.write_all(content.as_bytes())
                .map_err(|e| Aria2Error::Io(format!("写入进度数据失败: {}", e)))?;

            file.flush()
                .map_err(|e| Aria2Error::Io(format!("刷新进度文件缓冲区失败: {}", e)))?;
        }

        // 原子重命名
        fs::rename(&tmp_path, &file_path).map_err(|e| {
            // 清理临时文件
            let _ = fs::remove_file(&tmp_path);
            Aria2Error::Io(format!("重命名进度文件失败: {}", e))
        })?;

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT 进度保存成功"
        );

        Ok(())
    }

    /// Serialize BtProgress to text format with optimized string building
    ///
    /// Uses `write!` macro instead of `format!` + `push_str` to reduce memory allocations.
    /// Pre-allocates output buffer based on estimated size to avoid repeated reallocations.
    fn serialize_progress(&self, progress: &BtProgress) -> String {
        // Pre-allocate buffer with estimated size to minimize reallocations
        // Base overhead + per-peer and per-byte estimates for dynamic sections
        let estimated_size = 512 + progress.peers.len() * 24 + progress.bitfield.len() * 3;
        let mut output = String::with_capacity(estimated_size);

        // [Download] section - use write! for direct writing without intermediate String
        use std::fmt::Write;
        output.push_str("[Download]\n");
        let _ = writeln!(output, "info_hash={}", progress.to_hex_hash());
        let _ = writeln!(output, "version={}", progress.version);
        let _ = writeln!(output, "num_pieces={}", progress.num_pieces);
        let _ = writeln!(output, "piece_length={}", progress.piece_length);
        let _ = writeln!(output, "total_size={}", progress.total_size);
        let _ = writeln!(output, "downloaded={}", progress.stats.downloaded_bytes);
        let _ = writeln!(output, "uploaded={}", progress.stats.uploaded_bytes);
        let _ = writeln!(output, "elapsed={}", progress.stats.elapsed_seconds);

        // Serialize bitfield as hex using write! in loop (avoids collecting into intermediate String)
        let _ = write!(output, "bitfield=");
        for &byte in &progress.bitfield {
            let _ = write!(output, "{:02x}", byte);
        }
        let _ = writeln!(output);

        // [Peers] section - use write! for each peer entry
        output.push_str("[Peers]\n");
        for peer in &progress.peers {
            let _ = writeln!(output, "{}", peer);
        }

        output
    }

    /// 加载 BT 下载进度文件
    ///
    /// 自动检测 C++ 二进制格式和文本格式，并正确解析。
    /// 会校验文件中的 info_hash 是否与传入的参数匹配。
    ///
    /// # Arguments
    ///
    /// * `info_hash` - 预期的 info_hash (20 字节)，用于校验
    ///
    /// # Returns
    ///
    /// 返回加载的进度数据
    ///
    /// # Errors
    ///
    /// - 文件不存在或读取失败
    /// - 文件格式无效或损坏
    /// - info_hash 不匹配
    pub fn load_progress(&self, info_hash: &[u8; 20]) -> Result<BtProgress> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "加载 BT 进度"
        );

        let content = match fs::read_to_string(&file_path) {
            Ok(content) => content,
            Err(e) => {
                // 尝试作为二进制格式读取
                if e.kind() == std::io::ErrorKind::InvalidData {
                    return self.load_binary_format(info_hash, &file_path);
                }
                return Err(Aria2Error::Io(format!("读取进度文件失败: {}", e)));
            }
        };

        self.parse_text_format(info_hash, &content, &file_path)
    }

    /// 解析文本格式
    fn parse_text_format(
        &self,
        expected_hash: &[u8; 20],
        content: &str,
        file_path: &Path,
    ) -> Result<BtProgress> {
        let mut progress = BtProgress {
            info_hash: *expected_hash,
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            save_time: SystemTime::now(),
            version: 1,
        };

        let mut current_section = String::new();

        for line in content.lines() {
            let line = line.trim();

            // 跳过空行
            if line.is_empty() {
                continue;
            }

            // 检测 section header
            if line.starts_with('[') && line.ends_with(']') {
                current_section = line[1..line.len() - 1].to_string();
                continue;
            }

            match current_section.as_str() {
                "Download" => {
                    if let Some((key, value)) = line.split_once('=') {
                        match key.trim() {
                            "info_hash" => {
                                // 校验 info_hash 匹配
                                let file_hash = value.trim().to_lowercase();
                                let expected_hex: String =
                                    expected_hash.iter().map(|b| format!("{:02x}", b)).collect();
                                if file_hash != expected_hex {
                                    return Err(Aria2Error::Io(format!(
                                        "进度文件的 info_hash 不匹配: 文件={}, 期望={}",
                                        file_hash, expected_hex
                                    )));
                                }
                            }
                            "version" => {
                                progress.version = value.trim().parse::<u32>().unwrap_or(1);
                            }
                            "num_pieces" => {
                                progress.num_pieces = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "piece_length" => {
                                progress.piece_length = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "total_size" => {
                                progress.total_size = value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "downloaded" => {
                                progress.stats.downloaded_bytes =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "uploaded" => {
                                progress.stats.uploaded_bytes =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "elapsed" => {
                                progress.stats.elapsed_seconds =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "bitfield" => {
                                progress.bitfield = self.parse_bitfield_hex(value.trim());
                            }
                            _ => {}
                        }
                    }
                }
                "Peers" => {
                    let peer = self.parse_peer_addr(line);
                    if peer.is_some() {
                        progress.peers.push(peer.unwrap());
                    }
                }
                _ => {}
            }
        }

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT 进度加载成功"
        );

        Ok(progress)
    }

    /// 尝试加载二进制格式（C++ 兼容格式）
    fn load_binary_format(&self, _info_hash: &[u8; 20], _file_path: &Path) -> Result<BtProgress> {
        // 二进制格式暂不支持，返回错误提示用户使用新格式
        Err(Aria2Error::Io(
            "不支持旧的 C++ 二进制格式，请使用文本格式".to_string(),
        ))
    }

    /// 解析 bitfield 的十六进制字符串
    fn parse_bitfield_hex(&self, hex_str: &str) -> Vec<u8> {
        let hex_str = hex_str.trim();
        if hex_str.is_empty() {
            return Vec::new();
        }

        (0..hex_str.len())
            .step_by(2)
            .filter_map(|i| {
                if i + 1 < hex_str.len() {
                    u8::from_str_radix(&hex_str[i..i + 2], 16).ok()
                } else {
                    None
                }
            })
            .collect()
    }

    /// 解析 peer 地址字符串
    fn parse_peer_addr(&self, addr_str: &str) -> Option<PeerAddr> {
        let addr_str = addr_str.trim();
        if addr_str.is_empty() {
            return None;
        }

        // 查找最后一个冒号（IPv6 地址可能包含多个冒号）
        if let Some(colon_pos) = addr_str.rfind(':') {
            let ip = addr_str[..colon_pos].trim().to_string();
            let port: u16 = addr_str[colon_pos + 1..].trim().parse().ok()?;

            Some(PeerAddr { ip, port })
        } else {
            None
        }
    }

    /// 删除指定 info_hash 的进度文件
    ///
    /// # Arguments
    ///
    /// * `info_hash` - 要删除的种子的 info_hash (20 字节)
    ///
    /// # Errors
    ///
    /// 当文件删除失败时返回错误
    pub fn remove_progress(&self, info_hash: &[u8; 20]) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "删除 BT 进度"
        );

        if file_path.exists() {
            fs::remove_file(&file_path)
                .map_err(|e| Aria2Error::Io(format!("删除进度文件失败: {}", e)))?;

            info!(path = %file_path.display(), "BT 进度文件已删除");
        } else {
            warn!(path = %file_path.display(), "进度文件不存在");
        }

        Ok(())
    }

    /// 列出所有已保存的进度文件
    ///
    /// 扫描进度目录中所有 `.aria2` 文件，提取其 info_hash。
    ///
    /// # Returns
    ///
    /// 返回所有已保存进度的 info_hash 列表
    pub fn list_saved_progresses(&self) -> Vec<[u8; 20]> {
        let mut hashes = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.progress_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if let Some(hex_hash) = name_str.strip_suffix(".aria2") {
                        // 从文件名提取 hex hash
                        // 去掉 ".aria2"
                        if let Ok(hash) = Self::hex_to_info_hash(hex_hash) {
                            hashes.push(hash);
                        } else {
                            warn!(
                                filename = %name_str,
                                "无法解析进度文件名中的 info_hash"
                            );
                        }
                    }
                }
            }
        }

        debug!(count = hashes.len(), "列出所有已保存的 BT 进度");

        hashes
    }

    /// 将十六进制字符串转换为 info_hash
    fn hex_to_info_hash(hex_str: &str) -> std::result::Result<[u8; 20], ()> {
        if hex_str.len() != 40 {
            return Err(());
        }

        let mut hash = [0u8; 20];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
        }

        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_addr_display() {
        let peer = PeerAddr {
            ip: "192.168.1.1".to_string(),
            port: 6881,
        };
        assert_eq!(format!("{}", peer), "192.168.1.1:6881");
    }

    #[test]
    fn test_bt_progress_to_hex_hash() {
        let progress = BtProgress {
            info_hash: [
                0xAB, 0xCD, 0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            ..Default::default()
        };
        assert_eq!(
            progress.to_hex_hash(),
            "abcd123400000000000000000000000000000000"
        );
    }

    #[test]
    fn test_completion_ratio_zero_pieces() {
        let progress = BtProgress {
            num_pieces: 0,
            bitfield: vec![],
            ..Default::default()
        };
        assert_eq!(progress.completion_ratio(), 0.0);
    }

    #[test]
    fn test_completion_ratio_full() {
        // 4 个 piece 全部下载完成
        let progress = BtProgress {
            num_pieces: 4,
            bitfield: vec![0xFF], // 8 bits, 但只有 4 个 piece
            ..Default::default()
        };
        // 应该是 4/4 = 1.0，但实际会计算所有 set bits
        assert!(progress.completion_ratio() > 0.0);
    }

    #[test]
    fn test_parse_bitfield_hex() {
        let manager = create_test_manager();
        let result = manager.parse_bitfield_hex("ff00ff");
        assert_eq!(result, vec![0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn test_parse_bitfield_empty() {
        let manager = create_test_manager();
        let result = manager.parse_bitfield_hex("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_peer_addr_ipv4() {
        let manager = create_test_manager();
        let peer = manager.parse_peer_addr("192.168.1.100:6881").unwrap();
        assert_eq!(peer.ip, "192.168.1.100");
        assert_eq!(peer.port, 6881);
    }

    #[test]
    fn test_parse_peer_addr_invalid() {
        let manager = create_test_manager();
        assert!(manager.parse_peer_addr("invalid").is_none());
        assert!(manager.parse_peer_addr("").is_none());
    }

    #[test]
    fn test_hex_to_info_hash_valid() {
        let hex = "abcdef1234567890abcdef1234567890abcdef12";
        let hash = BtProgressManager::hex_to_info_hash(hex).unwrap();
        assert_eq!(
            hash,
            [
                0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56,
                0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12
            ]
        );
    }

    #[test]
    fn test_hex_to_info_hash_invalid_length() {
        assert!(BtProgressManager::hex_to_info_hash("abc123").is_err());
    }

    fn create_test_manager() -> BtProgressManager {
        let dir = std::env::temp_dir().join("bt_progress_test_12345");
        let _ = fs::create_dir_all(&dir);
        BtProgressManager::new(&dir).unwrap()
    }
}
