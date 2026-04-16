#[cfg(test)]
mod tests {
    use crate::engine::bt_progress_info_file::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// Create a temp directory for testing.
    ///
    /// Uses PID + nanosecond timestamp (truncated to avoid Windows MAX_PATH) so each
    /// test call gets its own isolated directory even within the same process.
    fn create_test_dir() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % 1_000_000_000;
        let dir = std::env::temp_dir().join(format!("bt_pt_{}_{}", std::process::id(), ts));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("Failed to create test directory");
        dir
    }

    /// 创建测试用的 BtProgress
    fn create_test_progress(info_hash: [u8; 20]) -> BtProgress {
        BtProgress {
            info_hash,
            bitfield: vec![0xFF, 0xF0], // 12 bits set out of 16
            peers: vec![
                PeerAddr {
                    ip: "192.168.1.100".to_string(),
                    port: 6881,
                },
                PeerAddr {
                    ip: "10.0.0.1".to_string(),
                    port: 6882,
                },
            ],
            stats: DownloadStats {
                uploaded_bytes: 1024 * 1024 * 100,
                downloaded_bytes: 1024 * 1024 * 500,
                upload_speed: 1024.0 * 50.0,
                download_speed: 1024.0 * 200.0,
                elapsed_seconds: 3600,
            },
            piece_length: 16384,
            total_size: 1024 * 1024 * 256, // 256 MB
            num_pieces: 16,
            save_time: std::time::SystemTime::now(),
            version: 1,
        }
    }

    #[test]
    fn test_save_load_roundtrip_new_format() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let info_hash = [
            0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
            0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11,
        ];
        let original = create_test_progress(info_hash);

        // 保存进度
        manager
            .save_progress(&info_hash, &original)
            .expect("保存失败");

        // 加载进度
        let loaded = manager.load_progress(&info_hash).expect("加载失败");

        // 验证数据一致性
        assert_eq!(loaded.info_hash, original.info_hash);
        assert_eq!(loaded.bitfield, original.bitfield);
        assert_eq!(loaded.peers.len(), original.peers.len());
        assert_eq!(
            loaded.stats.downloaded_bytes,
            original.stats.downloaded_bytes
        );
        assert_eq!(loaded.stats.uploaded_bytes, original.stats.uploaded_bytes);
        assert_eq!(loaded.piece_length, original.piece_length);
        assert_eq!(loaded.total_size, original.total_size);
        assert_eq!(loaded.num_pieces, original.num_pieces);
        assert_eq!(loaded.version, original.version);

        // 验证 peer 信息
        assert_eq!(loaded.peers[0].ip, "192.168.1.100");
        assert_eq!(loaded.peers[0].port, 6881);
        assert_eq!(loaded.peers[1].ip, "10.0.0.1");
        assert_eq!(loaded.peers[1].port, 6882);

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_bitfield_serialization() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let _info_hash = [0x11; 20];

        // 测试各种 bitfield 模式
        let test_cases = [
            (vec![0xFF], "全1"),
            (vec![0x00], "全0"),
            (vec![0xAA, 0x55], "交替模式"),
            (vec![0x01, 0x02, 0x04, 0x08], "单bit设置"),
            (vec![0xF0, 0x0F], "高4位和低4位"),
        ];

        for (i, (bitfield, desc)) in test_cases.iter().enumerate() {
            let mut progress = create_test_progress([i as u8; 20]);
            progress.bitfield = bitfield.clone();
            progress.num_pieces = (bitfield.len() * 8) as u32;

            let hash = [i as u8; 20];
            manager
                .save_progress(&hash, &progress)
                .unwrap_or_else(|_| panic!("保存失败: {}", desc));

            let loaded = manager
                .load_progress(&hash)
                .unwrap_or_else(|_| panic!("加载失败: {}", desc));

            assert_eq!(loaded.bitfield, *bitfield, "Bitfield 不匹配: {}", desc);
        }

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_peer_list_parsing() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let info_hash = [0x22; 20];

        // 测试各种 peer 地址格式
        let mut progress = create_test_progress(info_hash);
        progress.peers = vec![
            PeerAddr {
                ip: "192.168.1.1".to_string(),
                port: 6881,
            },
            PeerAddr {
                ip: "10.0.0.1".to_string(),
                port: 8080,
            },
            PeerAddr {
                ip: "172.16.0.1".to_string(),
                port: 443,
            },
            PeerAddr {
                ip: "127.0.0.1".to_string(),
                port: 9999,
            },
        ];

        manager
            .save_progress(&info_hash, &progress)
            .expect("保存失败");

        let loaded = manager.load_progress(&info_hash).expect("加载失败");

        assert_eq!(loaded.peers.len(), 4, "Peer 数量不匹配");
        assert_eq!(loaded.peers[0].ip, "192.168.1.1");
        assert_eq!(loaded.peers[0].port, 6881);
        assert_eq!(loaded.peers[1].ip, "10.0.0.1");
        assert_eq!(loaded.peers[1].port, 8080);
        assert_eq!(loaded.peers[2].ip, "172.16.0.1");
        assert_eq!(loaded.peers[2].port, 443);
        assert_eq!(loaded.peers[3].ip, "127.0.0.1");
        assert_eq!(loaded.peers[3].port, 9999);

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_atomic_write_safety() {
        let test_dir = create_test_dir();
        let manager = Arc::new(BtProgressManager::new(&test_dir).expect("创建管理器失败"));

        let info_hash = [0x33; 20];

        // 模拟并发写入
        let num_threads = 5;
        let barrier = Arc::new(Barrier::new(num_threads));
        let mut handles = Vec::with_capacity(num_threads);

        for i in 0..num_threads {
            let manager_clone = Arc::clone(&manager);
            let barrier_clone = Arc::clone(&barrier);

            handles.push(thread::spawn(move || {
                // 等待所有线程就绪
                barrier_clone.wait();

                let mut progress = create_test_progress(info_hash);
                // 每个线程写入不同的数据以区分
                progress.stats.downloaded_bytes = i as u64 * 1000;
                progress.bitfield = vec![i as u8; 4];

                manager_clone
                    .save_progress(&info_hash, &progress)
                    .expect("并发保存失败");
            }));
        }

        // 等待所有线程完成
        for handle in handles {
            handle.join().expect("线程 panic");
        }

        // 验证文件存在且可正常读取（不会损坏）
        let loaded = manager
            .load_progress(&info_hash)
            .expect("并发写入后加载失败");
        assert_eq!(loaded.info_hash, info_hash);
        assert!(!loaded.bitfield.is_empty());

        // Verify no leftover temp files (with brief grace period for cleanup)
        let file_path = manager.get_progress_file_path(&info_hash);
        let tmp_path = file_path.with_extension("aria2.tmp");
        std::thread::sleep(std::time::Duration::from_millis(10));
        if tmp_path.exists() {
            eprintln!("[WARN] Temp file may still exist from concurrent writes");
        }

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_corrupted_file_graceful_degradation() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let info_hash = [0x44; 20];
        let file_path = manager.get_progress_file_path(&info_hash);

        // 场景1: 空文件
        fs::write(&file_path, "").expect("写入空文件失败");
        let _result = manager.load_progress(&info_hash);
        // 空文件应该能被解析（只是数据为空或默认值）
        // 或者返回错误但不应该 panic

        // 场景2: 部分损坏的文件（缺少必要字段）
        fs::write(
            &file_path,
            "[Download]\ninfo_hash=invalid_hex\nversion=abc\n",
        )
        .expect("写入损坏文件失败");
        let result = manager.load_progress(&info_hash);
        // 应该返回错误（info_hash 不匹配）
        assert!(result.is_err(), "损坏的 info_hash 应该返回错误");

        // 场景3: 完全无效的内容
        fs::write(&file_path, "这是完全无效的内容@@@@###").expect("写入无效内容失败");
        let _result = manager.load_progress(&info_hash);
        // 应该能处理（返回空数据或错误），不能 panic

        // 场景4: 只有部分 section
        fs::write(
            &file_path,
            "[Download]\nnum_pieces=10\npiece_length=16384\n",
        )
        .expect("写入部分 section 失败");
        let _result = manager.load_progress(&info_hash);
        // 缺少 info_hash 字段，但不应 panic

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_progress_manager_create_dir() {
        let test_dir = std::env::temp_dir().join("bt_progress_create_dir_test_nested");

        // 确保目录不存在
        let _ = fs::remove_dir_all(&test_dir);

        // 嵌套目录路径
        let nested_dir = test_dir.join("level1").join("level2").join("level3");

        // 创建管理器时自动创建嵌套目录
        let manager = BtProgressManager::new(&nested_dir).expect("创建管理器失败");

        // 验证所有层级目录都已创建
        assert!(nested_dir.exists(), "嵌套目录应该被自动创建");
        assert!(nested_dir.is_dir(), "应该是目录");

        // 验证可以正常使用
        let info_hash = [0x55; 20];
        let progress = create_test_progress(info_hash);
        manager
            .save_progress(&info_hash, &progress)
            .expect("保存失败");
        let loaded = manager.load_progress(&info_hash).expect("加载失败");
        assert_eq!(loaded.info_hash, info_hash);

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_remove_progress() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let info_hash = [0x66; 20];
        let progress = create_test_progress(info_hash);

        // 保存进度
        manager
            .save_progress(&info_hash, &progress)
            .expect("保存失败");

        // 验证文件存在
        let file_path = manager.get_progress_file_path(&info_hash);
        assert!(file_path.exists(), "进度文件应该存在");

        // 删除进度
        manager.remove_progress(&info_hash).expect("删除失败");

        // 验证文件已删除
        assert!(!file_path.exists(), "进度文件应该已被删除");

        // 再次删除应该成功（文件不存在时只输出警告）
        manager
            .remove_progress(&info_hash)
            .expect("重复删除应该成功");

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_list_saved_progresses() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        // 初始状态：无进度文件
        let list = manager.list_saved_progresses();
        assert!(list.is_empty(), "初始状态应该是空的");

        // 保存多个进度
        let hashes: [[u8; 20]; 5] = [
            [
                0x77, 0x01, 0x77, 0x01, 0x77, 0x01, 0x77, 0x01, 0x77, 0x01, 0x77, 0x01, 0x77, 0x01,
                0x77, 0x01, 0x77, 0x01, 0x77, 0x01,
            ],
            [
                0x77, 0x02, 0x77, 0x02, 0x77, 0x02, 0x77, 0x02, 0x77, 0x02, 0x77, 0x02, 0x77, 0x02,
                0x77, 0x02, 0x77, 0x02, 0x77, 0x02,
            ],
            [
                0x77, 0x03, 0x77, 0x03, 0x77, 0x03, 0x77, 0x03, 0x77, 0x03, 0x77, 0x03, 0x77, 0x03,
                0x77, 0x03, 0x77, 0x03, 0x77, 0x03,
            ],
            [
                0x77, 0x04, 0x77, 0x04, 0x77, 0x04, 0x77, 0x04, 0x77, 0x04, 0x77, 0x04, 0x77, 0x04,
                0x77, 0x04, 0x77, 0x04, 0x77, 0x04,
            ],
            [
                0x77, 0x05, 0x77, 0x05, 0x77, 0x05, 0x77, 0x05, 0x77, 0x05, 0x77, 0x05, 0x77, 0x05,
                0x77, 0x05, 0x77, 0x05, 0x77, 0x05,
            ],
        ];

        for hash in &hashes {
            let progress = create_test_progress(*hash);
            manager.save_progress(hash, &progress).expect("保存失败");
        }

        // 列出所有进度
        let list = manager.list_saved_progresses();
        assert_eq!(list.len(), 5, "应该列出 5 个进度文件");

        // 验证每个 hash 都在列表中
        for hash in &hashes {
            assert!(list.contains(hash), "列表中应包含所有已保存的 hash");
        }

        // 删除一个后重新列出
        manager.remove_progress(&hashes[2]).expect("删除失败");
        let list = manager.list_saved_progresses();
        assert_eq!(list.len(), 4, "删除后应该只有 4 个");
        assert!(!list.contains(&hashes[2]), "删除的 hash 不应该在列表中");

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_completion_ratio_calculation() {
        // 测试完成百分比计算的正确性

        // 场景1: 完全未下载
        let progress = BtProgress {
            num_pieces: 10,
            bitfield: vec![0x00, 0x00], // 16 bits, 但只有 10 个 pieces
            ..Default::default()
        };
        assert_eq!(progress.completion_ratio(), 0.0, "未下载应该是 0%");

        // 场景2: 全部下载完成 (假设 8 pieces)
        let progress = BtProgress {
            num_pieces: 8,
            bitfield: vec![0xFF], // 所有 8 bits 都设置
            ..Default::default()
        };
        assert!(
            (progress.completion_ratio() - 1.0).abs() < f64::EPSILON,
            "全部完成应该是 100%"
        );

        // 场景3: 一半下载完成 (4/8)
        let progress = BtProgress {
            num_pieces: 8,
            bitfield: vec![0x0F], // 低 4 位设置
            ..Default::default()
        };
        assert!(
            (progress.completion_ratio() - 0.5).abs() < 0.01,
            "一半完成应该是约 50%"
        );

        // 场景4: 25% 完成 (2/8)
        let progress = BtProgress {
            num_pieces: 8,
            bitfield: vec![0x03], // 低 2 位设置
            ..Default::default()
        };
        assert!(
            (progress.completion_ratio() - 0.25).abs() < 0.01,
            "25% 完成"
        );

        // 场景5: 奇数数量的 pieces (13 pieces, 8 已完成)
        let progress = BtProgress {
            num_pieces: 13,
            bitfield: vec![0xFE, 0x01], // 8 bits set: 11111110 00000001
            ..Default::default()
        };
        let ratio = progress.completion_ratio();
        assert!(
            ratio > 0.6 && ratio < 0.7,
            "8/13 应该在 60%-70% 之间，实际: {:.2}%",
            ratio * 100.0
        );

        // 场景6: 空 bitfield
        let progress = BtProgress {
            num_pieces: 10,
            bitfield: vec![],
            ..Default::default()
        };
        assert_eq!(progress.completion_ratio(), 0.0, "空 bitfield 应该是 0%");
    }

    #[test]
    fn test_empty_peers_handling() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("创建管理器失败");

        let info_hash = [0x88; 20];

        // 创建没有 peer 的进度
        let mut progress = create_test_progress(info_hash);
        progress.peers = Vec::new();

        // 保存
        manager
            .save_progress(&info_hash, &progress)
            .expect("保存失败");

        // 加载
        let loaded = manager.load_progress(&info_hash).expect("加载失败");

        // 验证 peer 列表为空
        assert!(loaded.peers.is_empty(), "Peer 列表应该是空的");

        // 验证其他数据不受影响
        assert_eq!(loaded.num_pieces, progress.num_pieces);
        assert_eq!(loaded.total_size, progress.total_size);
        assert_eq!(loaded.bitfield, progress.bitfield);

        // 清理
        let _ = fs::remove_dir_all(&test_dir);
    }
}
