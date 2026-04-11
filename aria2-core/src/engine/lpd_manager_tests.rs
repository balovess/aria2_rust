#![cfg(test)]
//! LPD 管理器单元测试
//!
//! 测试覆盖：
//! - LpdAnnounce 序列化/反序列化
//! - LpdManager 注册/注销下载
//! - 报文处理和 peer 发现
//! - 过期清理
//! - 启用/禁用切换

mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::time::Duration;

    use tokio::time::Duration as TokioDuration;

    use crate::engine::lpd_manager::{LPD_ANNOUNCE_MIN_SIZE, LpdAnnounce, LpdManager, LpdPeer};

    /// 创建测试用的 info_hash
    ///
    /// 使用种子值生成确定性的 20 字节 hash，便于测试。
    fn make_test_hash(seed: u8) -> [u8; 20] {
        let mut hash = [0u8; 20];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        hash
    }

    // ==================== LpdAnnounce 序列化测试 ====================

    #[test]
    fn test_lpd_announce_serialize() {
        let from_hash = make_test_hash(0x01);
        let to_hash = make_test_hash(0x02);
        let announce = LpdAnnounce {
            from_hash,
            to_hash,
            port: 6881,
        };

        let bytes = announce.to_bytes();

        // 验证长度为 43 字节
        assert_eq!(
            bytes.len(),
            LPD_ANNOUNCE_MIN_SIZE,
            "序列化后的报文应为 43 字节"
        );

        // 验证 from_hash（前 20 字节）
        assert_eq!(&bytes[0..20], &from_hash[..], "from_hash 应正确序列化");

        // 验证 to_hash（第 21-40 字节）
        assert_eq!(&bytes[20..40], &to_hash[..], "to_hash 应正确序列化");

        // 验证 port（Big Endian）: 6881 = 0x1AA1
        let serialized_port = u16::from_be_bytes([bytes[40], bytes[41]]);
        assert_eq!(serialized_port, 6881, "port 序列化后应还原为 6881");

        // 验证终止符
        assert_eq!(bytes[42], 0, "终止符应为 0");
    }

    #[test]
    fn test_lpd_announce_deserialize() {
        let from_hash = make_test_hash(0xAA);
        let to_hash = make_test_hash(0xBB);
        let port: u16 = 12345;

        // 手动构造数据
        let mut data = Vec::with_capacity(43);
        data.extend_from_slice(&from_hash);
        data.extend_from_slice(&to_hash);
        data.extend_from_slice(&port.to_be_bytes());
        data.push(0);

        let announce = LpdAnnounce::from_bytes(&data).expect("应该成功解析");

        assert_eq!(announce.from_hash, from_hash, "from_hash 应正确还原");
        assert_eq!(announce.to_hash, to_hash, "to_hash 应正确还原");
        assert_eq!(announce.port, port, "port 应正确还原");
    }

    #[test]
    fn test_lpd_announce_truncated_data() {
        // 测试各种不足长度的数据（0-42 字节都不应成功解析）
        for len in 0..LPD_ANNOUNCE_MIN_SIZE {
            let data = vec![0xABu8; len];
            let result = LpdAnnounce::from_bytes(&data);

            assert!(result.is_none(), "长度为 {} 的数据不应该被成功解析", len);
        }
    }

    #[test]
    fn test_lpd_announce_minimal_size() {
        // 精确 43 字节数据应正常解析
        let data = vec![0xABu8; LPD_ANNOUNCE_MIN_SIZE];
        let result = LpdAnnounce::from_bytes(&data);

        assert!(result.is_some(), "精确 43 字节应该能正常解析");

        let announce = result.unwrap();
        assert_eq!(announce.from_hash, [0xABu8; 20]);
        assert_eq!(announce.to_hash, [0xABu8; 20]);

        // port 应该是 0xABAB（Big Endian: [0xAB, 0xAB]）
        assert_eq!(announce.port, 0xABAB);
    }

    // ==================== LpdManager 注册/注销测试 ====================

    #[tokio::test]
    async fn test_register_and_unregister_download() {
        let manager = LpdManager::new(true, 6881);
        let hash = make_test_hash(0x10);

        // 注册前应该是空的
        assert_eq!(
            manager.active_download_count().await,
            0,
            "初始状态不应有活跃下载"
        );

        // 注册下载
        manager.register_download(hash);

        // 等待异步操作完成
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        assert_eq!(
            manager.active_download_count().await,
            1,
            "注册后应有 1 个活跃下载"
        );

        // 注销下载
        manager.unregister_download(hash);

        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        assert_eq!(
            manager.active_download_count().await,
            0,
            "注销后不应有活跃下载"
        );
    }

    #[tokio::test]
    async fn test_double_register_no_duplicate() {
        let manager = LpdManager::new(true, 6881);
        let hash = make_test_hash(0x20);

        // 同一 hash 注册两次
        manager.register_download(hash);
        manager.register_download(hash);

        // 等待异步操作完成
        tokio::time::sleep(TokioDuration::from_millis(200)).await;

        // 应该只有一个（去重）
        assert_eq!(
            manager.active_download_count().await,
            1,
            "重复注册不应产生重复条目"
        );
    }

    // ==================== 报文处理和 peer 发现测试 ====================

    #[tokio::test]
    async fn test_handle_valid_packet_adds_peer() {
        let manager = LpdManager::new(true, 6881);
        let hash = make_test_hash(0x30);

        // 先注册这个 hash
        manager.register_download(hash);
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        // 构造有效报文（to_hash 匹配已注册的 hash）
        let announce = LpdAnnounce {
            from_hash: make_test_hash(0xFF),
            to_hash: hash,
            port: 9999,
        };
        let packet_data = announce.to_bytes();
        let src_addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 12345);

        // 处理报文
        manager.handle_incoming_packet(&packet_data, src_addr).await;

        // 应该有一个 peer 被发现
        assert_eq!(
            manager.discovered_peer_count().await,
            1,
            "有效报文应添加一个 peer"
        );

        // 验证 peer 详情
        let peers = manager.get_discovered_peers(hash).await;
        assert_eq!(peers.len(), 1);
        assert_eq!(
            peers[0].addr.ip(),
            &Ipv4Addr::new(192, 168, 1, 100),
            "peer IP 应匹配源地址"
        );
        assert_eq!(peers[0].addr.port(), 9999, "peer 端口应来自报文");
    }

    #[tokio::test]
    async fn test_handle_packet_mismatched_hash() {
        let manager = LpdManager::new(true, 6881);
        let registered_hash = make_test_hash(0x40);
        let other_hash = make_test_hash(0x50);

        // 只注册 registered_hash
        manager.register_download(registered_hash);
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        // 发送不匹配的 to_hash 报文
        let announce = LpdAnnounce {
            from_hash: other_hash,
            to_hash: other_hash, // 不匹配任何已注册的 hash
            port: 8888,
        };
        let packet_data = announce.to_bytes();
        let src_addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 54321);

        manager.handle_incoming_packet(&packet_data, src_addr).await;

        // 不应该有 peer 被添加
        assert_eq!(
            manager.discovered_peer_count().await,
            0,
            "不匹配的 to_hash 不应添加 peer"
        );
    }

    #[tokio::test]
    async fn test_get_discovered_peers_filters_by_hash() {
        let manager = LpdManager::new(true, 6881);
        let hash_a = make_test_hash(0x60);
        let hash_b = make_test_hash(0x70);

        // 注册两个不同的 hash
        manager.register_download(hash_a);
        manager.register_download(hash_b);
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        // 为不同 hash 添加 peer
        let addr_a = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 10), 11111);
        let addr_b = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 22222);

        let announce_a = LpdAnnounce {
            from_hash: hash_a,
            to_hash: hash_a,
            port: 11111,
        };
        manager
            .handle_incoming_packet(&announce_a.to_bytes(), addr_a)
            .await;

        let announce_b = LpdAnnounce {
            from_hash: hash_b,
            to_hash: hash_b,
            port: 22222,
        };
        manager
            .handle_incoming_packet(&announce_b.to_bytes(), addr_b)
            .await;

        // 按 hash 过滤查询
        let peers_a = manager.get_discovered_peers(hash_a).await;
        let peers_b = manager.get_discovered_peers(hash_b).await;

        assert_eq!(peers_a.len(), 1, "hash_a 应有 1 个 peer");
        assert_eq!(peers_b.len(), 1, "hash_b 应有 1 个 peer");
        assert_ne!(
            peers_a[0].addr, peers_b[0].addr,
            "不同 hash 的 peer 地址应不同"
        );
    }

    // ==================== 过期清理测试 ====================

    #[tokio::test]
    async fn test_cleanup_expired_peers() {
        let manager = LpdManager::new(true, 6881);
        let hash = make_test_hash(0x80);

        manager.register_download(hash);
        tokio::time::sleep(TokioDuration::from_millis(100)).await;

        // 添加一个 peer
        let announce = LpdAnnounce {
            from_hash: hash,
            to_hash: hash,
            port: 33333,
        };
        let addr = SocketAddrV4::new(Ipv4Addr::new(172, 16, 0, 1), 33333);
        manager
            .handle_incoming_packet(&announce.to_bytes(), addr)
            .await;

        assert_eq!(
            manager.discovered_peer_count().await,
            1,
            "清理前应有 1 个 peer"
        );

        // 使用极短的 TTL 清理（纳秒级，确保立即过期）
        manager.cleanup_expired_peers(Duration::from_nanos(1)).await;

        // peer 应该被移除（因为 discovered_at 已经是"过去式"）
        assert_eq!(
            manager.discovered_peer_count().await,
            0,
            "过期 peer 应被清除"
        );
    }

    // ==================== 启用/禁用测试 ====================

    #[test]
    fn test_enabled_toggle() {
        let manager = LpdManager::new(false, 6881);

        // 默认禁用
        assert!(!manager.is_enabled(), "默认应禁用");

        // 启用
        manager.set_enabled(true);
        assert!(manager.is_enabled(), "启用后应返回 true");

        // 再次禁用
        manager.set_enabled(false);
        assert!(!manager.is_enabled(), "禁用后应返回 false");
    }

    // ==================== 构建 Announces 测试 ====================

    #[tokio::test]
    async fn test_build_announces_multiple_downloads() {
        let manager = LpdManager::new(true, 6881);

        let hash1 = make_test_hash(0x90);
        let hash2 = make_test_hash(0xA0);
        let hash3 = make_test_hash(0xB0);

        // 注册多个下载
        manager.register_download(hash1);
        manager.register_download(hash2);
        manager.register_download(hash3);
        tokio::time::sleep(TokioDuration::from_millis(150)).await;

        // 构建所有 announces
        let announces = manager.build_announces().await;

        assert_eq!(announces.len(), 3, "3 个活跃下载应产生 3 个 announce");

        // 验证每个 announce 的字段
        for announce in &announces {
            assert_eq!(announce.port, 6881, "announce 端口应等于 listen_port");
            assert_eq!(
                announce.from_hash, announce.to_hash,
                "自身广播时 from_hash 和 to_hash 应相同"
            );
        }

        // 验证所有 hash 都存在
        let hashes: Vec<&[u8; 20]> = announces.iter().map(|a| &a.to_hash).collect();
        assert!(hashes.contains(&&hash1), "hash1 应在 announces 中");
        assert!(hashes.contains(&&hash2), "hash2 应在 announces 中");
        assert!(hashes.contains(&&hash3), "hash3 应在 announces 中");
    }

    // ==================== LpdPeer 辅助方法测试 ====================

    #[test]
    fn test_lpd_peer_is_expired() {
        let peer = LpdPeer {
            addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345),
            discovered_at: std::time::Instant::now(),
            source_hash: make_test_hash(0xC0),
        };

        // 未过期（TTL 很长）
        assert!(
            !peer.is_expired(Duration::from_secs(3600)),
            "刚发现的 peer 在长 TTL 下不应过期"
        );

        // 已过期（TTL 为零）
        assert!(
            peer.is_expired(Duration::ZERO),
            "peer 在 TTL=0 时应被视为过期"
        );
    }
}
