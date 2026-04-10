#[cfg(test)]
mod tests {
    use crate::engine::bt_download_command::BtDownloadCommand;
    use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
    use crate::engine::bt_upload_session::PieceDataProvider;
    use crate::engine::peer_stats::PeerStats;
    use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
    use crate::request::request_group::{DownloadOptions, GroupId};
    use crate::engine::multi_file_layout::MultiFileLayout;
    use std::net::SocketAddr;

    fn build_test_torrent() -> Vec<u8> {
        let mut v = Vec::new();

        v.push(b'd');

        let url = b"http://tracker.example.com/announce";
        v.extend_from_slice(format!("8:announce{}:", url.len()).as_bytes());
        v.extend_from_slice(url);

        v.extend_from_slice(b"4:info");
        v.push(b'd');

        v.extend_from_slice(b"6:lengthi0e");

        v.extend_from_slice(b"4:name4:test");

        v.extend_from_slice(b"12:piece lengthi16384e");

        v.extend_from_slice(b"6:pieces20:");
        v.extend_from_slice(&[0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d,
                           0x32, 0x55, 0xbf, 0xef, 0x95, 0x60, 0x18, 0x90,
                           0xaf, 0xd8, 0x07, 0x09]);

        v.push(b'e');

        v.push(b'e');

        v
    }

    fn create_test_command() -> BtDownloadCommand {
        let torrent_bytes = build_test_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(1);
        BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
            .expect("Failed to create test command")
    }

    #[test]
    fn test_bt_seed_manager_integration_choking_algo_none_by_default() {
        let cmd = create_test_command();
        assert!(cmd.choking_algo.is_none(), "choking_algo should be None by default");
    }

    #[test]
    fn test_download_side_choke_tracking() {
        let mut cmd = create_test_command();

        let config = ChokingConfig {
            max_upload_slots: 4,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "192.168.1.10:6881".parse().unwrap();
        let peer = PeerStats::new([0xAA; 20], addr);
        algo.add_peer(peer);
        cmd.choking_algo = Some(algo);

        assert!(cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking);

        cmd.on_peer_unchoke(0);
        assert!(
            !cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking,
            "peer_choking should be false after on_peer_unchoke"
        );

        cmd.on_peer_choke(0);
        assert!(
            cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking,
            "peer_choking should be true after on_peer_choke"
        );
    }

    #[test]
    fn test_download_side_select_best_peer_prefers_unchoked() {
        let mut cmd = create_test_command();

        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        let addr0: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let mut p0 = PeerStats::new([0x01; 20], addr0);
        p0.peer_choking = false;
        p0.download_speed = 100000.0;

        let addr1: SocketAddr = "10.0.0.2:6881".parse().unwrap();
        let mut p1 = PeerStats::new([0x02; 20], addr1);
        p1.peer_choking = true;
        p1.download_speed = 500000.0;

        let addr2: SocketAddr = "10.0.0.3:6881".parse().unwrap();
        let mut p2 = PeerStats::new([0x03; 20], addr2);
        p2.peer_choking = false;
        p2.is_snubbed = true;
        p2.download_speed = 80000.0;

        algo.add_peer(p0);
        algo.add_peer(p1);
        algo.add_peer(p2);
        cmd.choking_algo = Some(algo);

        let best = cmd.select_best_peer_for_request();
        assert_eq!(best, Some(0), "Should prefer unchoked+not-snubbed peer (peer 0)");
    }

    #[test]
    fn test_snubbed_peer_handling() {
        let mut cmd = create_test_command();

        let config = ChokingConfig {
            snubbed_timeout_secs: 1,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "172.16.0.5:6881".parse().unwrap();
        let peer = PeerStats::new([0xBB; 20], addr);
        algo.add_peer(peer);
        cmd.choking_algo = Some(algo);

        let snubbed = cmd.check_snubbed_peers();
        assert!(snubbed.is_empty(), "No peers should be snubbed initially");

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let snubbed = cmd.check_snubbed_peers();
        assert_eq!(snubbed.len(), 1, "Peer should be snubbed after timeout");
        assert_eq!(snubbed[0], 0);

        cmd.on_data_received_from_peer(0, 1024);
        assert!(
            !cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap().is_snubbed,
            "Receiving data should reset snubbed status"
        );
    }

    #[test]
    fn test_add_peer_to_tracking() {
        let mut cmd = create_test_command();

        let options = DownloadOptions {
            bt_max_upload_slots: Some(4),
            ..Default::default()
        };
        let gid = GroupId::new(2);
        let torrent_bytes = build_test_torrent();
        cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, None)
            .expect("Failed to create command with choking config");

        assert!(cmd.choking_algo.is_some());

        let addr1: SocketAddr = "192.168.1.20:6881".parse().unwrap();
        let idx1 = cmd.add_peer_to_tracking([0x11; 8], addr1);
        assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 1);

        let addr2: SocketAddr = "192.168.1.21:6881".parse().unwrap();
        let idx2 = cmd.add_peer_to_tracking([0x22; 8], addr2);
        assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 2);
        assert_ne!(idx1, idx2, "Different peers should get different indices");
    }

    #[test]
    fn test_download_command_backward_compat_no_choking_config() {
        let mut cmd = create_test_command();
        assert!(cmd.choking_algo.is_none());

        cmd.on_peer_choke(0);
        cmd.on_peer_unchoke(0);
        cmd.on_data_received_from_peer(0, 1024);
        let best = cmd.select_best_peer_for_request();
        assert_eq!(best, None, "Should return None when no algorithm configured");
        let snubbed = cmd.check_snubbed_peers();
        assert!(snubbed.is_empty());
    }

    fn build_multi_file_torrent() -> Vec<u8> {
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
        use std::collections::BTreeMap;

        let file1_path = BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir1".to_vec()),
            BencodeValue::Bytes(b"file1.txt".to_vec()),
        ]);
        let mut file1_dict = BTreeMap::new();
        file1_dict.insert(b"length".to_vec(), BencodeValue::Int(500));
        file1_dict.insert(b"path".to_vec(), file1_path);

        let file2_path = BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir2".to_vec()),
            BencodeValue::Bytes(b"file2.dat".to_vec()),
        ]);
        let mut file2_dict = BTreeMap::new();
        file2_dict.insert(b"length".to_vec(), BencodeValue::Int(524));
        file2_dict.insert(b"path".to_vec(), file2_path);

        let files_list = BencodeValue::List(vec![
            BencodeValue::Dict(file1_dict),
            BencodeValue::Dict(file2_dict),
        ]);

        let mut info_dict = BTreeMap::new();
        info_dict.insert(b"name".to_vec(), BencodeValue::Bytes(b"multitest".to_vec()));
        info_dict.insert(b"files".to_vec(), files_list);
        info_dict.insert(b"piece length".to_vec(), BencodeValue::Int(512));

        let mut pieces_hash = Vec::new();
        pieces_hash.extend_from_slice(&[0u8; 20]);
        pieces_hash.extend_from_slice(&[1u8; 20]);
        info_dict.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_hash));

        let mut root_dict = BTreeMap::new();
        root_dict.insert(b"announce".to_vec(), BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()));
        root_dict.insert(b"info".to_vec(), BencodeValue::Dict(info_dict));

        BencodeValue::Dict(root_dict).encode()
    }

    #[test]
    fn test_multi_file_layout_created_for_multi_torrent() {
        let torrent_bytes = build_multi_file_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(100);
        let cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, Some("d:/tmp/multitest"))
            .expect("Failed to create command from multi-file torrent");

        assert!(cmd.multi_file_layout.is_some(), "multi_file_layout should be Some for multi-file torrent");
        let layout = cmd.multi_file_layout.as_ref().unwrap();
        assert!(layout.is_multi_file());
        assert_eq!(layout.num_files(), 2);
        assert_eq!(layout.total_size(), 1024);
    }

    #[test]
    fn test_single_file_no_layout() {
        let cmd = create_test_command();

        assert!(cmd.multi_file_layout.is_none(), "multi_file_layout should be None for single-file torrent");
    }

    #[test]
    fn test_is_multi_file_accessor() {
        let single_cmd = create_test_command();
        assert!(!single_cmd.is_multi_file(), "Single-file torrent should return false");

        let multi_bytes = build_multi_file_torrent();
        let options = DownloadOptions::default();
        let gid = GroupId::new(101);
        let multi_cmd = BtDownloadCommand::new(gid, &multi_bytes, &options, Some("d:/tmp/test_acc"))
            .expect("Failed to create multi-file command");
        assert!(multi_cmd.is_multi_file(), "Multi-file torrent should return true");

        assert!(multi_cmd.get_multi_file_layout().is_some());
        assert!(create_test_command().get_multi_file_layout().is_none());
    }

    #[tokio::test]
    async fn test_write_piece_to_multi_files_basic() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "write_test".to_string(),
            piece_length: 256,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 200, path: vec!["sub".to_string(), "a.bin".to_string()] },
                FileEntry { length: 312, path: vec!["sub".to_string(), "b.bin".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("mf_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&base_dir).unwrap();
        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();

        layout.create_directories().unwrap();

        let piece_data: Vec<u8> = (0..=255u8).collect();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            BtDownloadCommand::write_piece_to_multi_files(
                &layout,
                0,
                &piece_data,
                layout.piece_length(),
            ),
        ).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("write_piece_to_multi_files failed: {}", e),
            Err(_) => panic!("write_piece_to_multi_files timed out after 10s"),
        }

        let file_a = base_dir.join("sub").join("a.bin");
        let file_b = base_dir.join("sub").join("b.bin");
        assert!(file_a.exists(), "File a.bin should exist after write");
        assert!(file_b.exists(), "File b.bin should exist after write");

        let a_contents = std::fs::read(&file_a).unwrap();
        assert_eq!(a_contents.len(), 200, "a.bin should have 200 bytes");
        assert_eq!(&a_contents[..], &piece_data[..200], "a.bin contents should match first 200 bytes of piece");

        let b_contents = std::fs::read(&file_b).unwrap();
        assert_eq!(b_contents.len(), 56, "b.bin should have 56 bytes (remaining from piece 0)");
        assert_eq!(&b_contents[..], &piece_data[200..256], "b.bin contents should match remaining bytes");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_write_piece_resolve_logic() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "resolve_test".to_string(),
            piece_length: 128,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 100, path: vec!["x".to_string(), "f1.dat".to_string()] },
                FileEntry { length: 156, path: vec!["x".to_string(), "f2.dat".to_string()] },
            ]),
            private: None,
        };

        let base = std::path::PathBuf::from("d:/tmp/resolve_test");
        let layout = MultiFileLayout::from_info_dict(&info, &base).unwrap();
        assert_eq!(layout.num_files(), 2);
        assert_eq!(layout.total_size(), 256);
        assert!(layout.is_multi_file());

        let r0 = layout.resolve_file_offset(0, 0);
        assert_eq!(r0, Some((0, 0)), "Piece 0 offset 0 should map to file 0 offset 0");

        let r1 = layout.resolve_file_offset(0, 99);
        assert_eq!(r1, Some((0, 99)), "Piece 0 offset 99 should map to file 0 offset 99");

        let r2 = layout.resolve_file_offset(0, 100);
        assert_eq!(r2, Some((1, 0)), "Piece 0 offset 100 should map to file 1 offset 0 (cross-file boundary)");

        let r3 = layout.resolve_file_offset(0, 127);
        assert_eq!(r3, Some((1, 27)), "Piece 0 offset 127 should map to file 1 offset 27");

        let r4 = layout.resolve_file_offset(1, 0);
        assert_eq!(r4, Some((1, 28)), "Piece 1 offset 0 should map to file 1 offset 28");

        let r5 = layout.resolve_file_offset(1, 127);
        assert_eq!(r5, Some((1, 155)), "Piece 1 offset 127 should map to file 1 offset 155");

        let r_oob = layout.resolve_file_offset(1, 128);
        assert_eq!(r_oob, None, "Out-of-range offset should return None");
    }

    #[test]
    fn test_multi_file_piece_provider_reads_correct_file() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "provider_test".to_string(),
            piece_length: 128,
            pieces: vec![[0u8; 20], [1u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 100, path: vec!["p".to_string(), "a.dat".to_string()] },
                FileEntry { length: 156, path: vec!["p".to_string(), "b.dat".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("mfp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(base_dir.join("p")).unwrap();

        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
        layout.create_directories().unwrap();

        let file_a = layout.file_absolute_path(0).unwrap().to_path_buf();
        let file_b = layout.file_absolute_path(1).unwrap().to_path_buf();

        let data_a: Vec<u8> = (0..100u8).collect();
        let data_b: Vec<u8> = (100..=255u8).collect();
        std::fs::write(&file_a, &data_a).unwrap();
        std::fs::write(&file_b, &data_b).unwrap();

        let provider = FileBackedPieceProvider::new(
            base_dir.clone(),
            128,
            2,
            Some(layout),
        );

        let result = provider.get_piece_data(0, 0, 10);
        assert!(result.is_some(), "Should read from file a at offset 0");
        assert_eq!(result.unwrap(), (0..10u8).collect::<Vec<u8>>(), "First 10 bytes should match file a");

        let result_mid = provider.get_piece_data(0, 50, 50);
        assert!(result_mid.is_some());
        assert_eq!(result_mid.unwrap(), (50..100u8).collect::<Vec<u8>>(), "Bytes 50-99 from file a");

        let result_cross = provider.get_piece_data(0, 95, 5);
        assert!(result_cross.is_some());
        assert_eq!(result_cross.unwrap(), (95..100u8).collect::<Vec<u8>>(), "Last 5 bytes of file a");

        let result_b = provider.get_piece_data(1, 28, 50);
        assert!(result_b.is_some());
        assert_eq!(result_b.unwrap(), (156u8..=205u8).collect::<Vec<u8>>(), "Piece 1 offset 28 = global byte 156 = file b offset 56");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_single_file_piece_provider_unchanged() {
        let tmp = std::env::temp_dir().join(format!("sfp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("single.bin");
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&file_path, &data).unwrap();

        let provider = FileBackedPieceProvider::new(file_path.clone(), 128, 2, None);

        let result = provider.get_piece_data(0, 0, 16);
        assert!(result.is_some(), "Single-file provider should read successfully");
        assert_eq!(result.unwrap(), (0..16u8).collect::<Vec<u8>>(), "First 16 bytes should match");

        let result_mid = provider.get_piece_data(0, 64, 32);
        assert!(result_mid.is_some());
        assert_eq!(result_mid.unwrap(), (64..96u8).collect::<Vec<u8>>(), "Mid-piece read should match");

        let result_p1 = provider.get_piece_data(1, 0, 32);
        assert!(result_p1.is_some());
        assert_eq!(result_p1.unwrap(), (128..160u8).collect::<Vec<u8>>(), "Piece 1 offset 0 = byte 128");

        let result_end = provider.get_piece_data(1, 127, 1);
        assert!(result_end.is_some());
        assert_eq!(result_end.unwrap(), vec![255u8], "Last byte should be 255");

        assert_eq!(provider.num_pieces(), 2);
        assert!(provider.has_piece(0));
        assert!(provider.has_piece(1));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_multi_file_cross_boundary_read() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "cross_boundary_test".to_string(),
            piece_length: 256,
            pieces: vec![[0u8; 20], [1u8; 20], [2u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 150, path: vec!["cb".to_string(), "f1.bin".to_string()] },
                FileEntry { length: 150, path: vec!["cb".to_string(), "f2.bin".to_string()] },
                FileEntry { length: 100, path: vec!["cb".to_string(), "f3.bin".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("cb_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(base_dir.join("cb")).unwrap();

        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
        layout.create_directories().unwrap();

        let file1 = layout.file_absolute_path(0).unwrap().to_path_buf();
        let file2 = layout.file_absolute_path(1).unwrap().to_path_buf();
        let file3 = layout.file_absolute_path(2).unwrap().to_path_buf();

        let data1: Vec<u8> = (0..150u8).collect();
        let data2: Vec<u8> = (150..300).map(|i: u64| i as u8).collect();
        let data3: Vec<u8> = (300..400).map(|i: u64| i as u8).collect();

        std::fs::write(&file1, &data1).unwrap();
        std::fs::write(&file2, &data2).unwrap();
        std::fs::write(&file3, &data3).unwrap();

        let provider = FileBackedPieceProvider::new(
            base_dir.clone(),
            256,
            3,
            Some(layout),
        );

        let result = provider.get_piece_data(0, 140, 10);
        assert!(result.is_some(), "Read within file1 should succeed");
        let data = result.unwrap();
        assert_eq!(data.len(), 10, "Should read exactly 10 bytes");
        assert_eq!(data, (140..150u8).collect::<Vec<u8>>(), "Bytes 140-149 from file1");

        let result_p1 = provider.get_piece_data(1, 0, 100);
        assert!(result_p1.is_some(), "Read from piece 1 should work");
        let data_p1 = result_p1.unwrap();
        assert_eq!(data_p1.len(), 100);

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_large_offset_and_edge_cases() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "edge_case_test".to_string(),
            piece_length: 1024,
            pieces: vec![[0u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 800, path: vec!["ec".to_string(), "big.dat".to_string()] },
                FileEntry { length: 224, path: vec!["ec".to_string(), "small.dat".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("ec_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(base_dir.join("ec")).unwrap();

        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
        layout.create_directories().unwrap();

        let big_file = layout.file_absolute_path(0).unwrap().to_path_buf();
        let small_file = layout.file_absolute_path(1).unwrap().to_path_buf();

        let big_data: Vec<u8> = (0..800).map(|i: u64| (i % 256) as u8).collect();
        let small_data: Vec<u8> = (800..1024).map(|i: u64| (i % 256) as u8).collect();

        std::fs::write(&big_file, &big_data).unwrap();
        std::fs::write(&small_file, &small_data).unwrap();

        let provider = FileBackedPieceProvider::new(
            base_dir.clone(),
            1024,
            1,
            Some(layout),
        );

        let result_start = provider.get_piece_data(0, 0, 1);
        assert!(result_start.is_some());
        assert_eq!(result_start.unwrap(), vec![0u8], "First byte should be 0");

        let result_near_end = provider.get_piece_data(0, 1023, 1);
        assert!(result_near_end.is_some());
        assert_eq!(result_near_end.unwrap(), vec![255u8], "Last byte should be 255");

        let result_zero_len = provider.get_piece_data(0, 500, 0);
        assert!(result_zero_len.is_some(), "Zero-length read should return empty");
        assert_eq!(result_zero_len.unwrap().len(), 0, "Zero-length read should return empty vec");

        let result_full_piece = provider.get_piece_data(0, 0, 512);
        assert!(result_full_piece.is_some());
        assert_eq!(result_full_piece.unwrap().len(), 512, "Half piece read should return 512 bytes");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_provider_error_handling() {
        use aria2_protocol::bittorrent::torrent::parser::{InfoDict, FileEntry};

        let info = InfoDict {
            name: "error_test".to_string(),
            piece_length: 128,
            pieces: vec![[0u8; 20]],
            length: None,
            files: Some(vec![
                FileEntry { length: 100, path: vec!["err".to_string(), "exists.dat".to_string()] },
                FileEntry { length: 50, path: vec!["err".to_string(), "missing.dat".to_string()] },
            ]),
            private: None,
        };

        let base_dir = std::env::temp_dir().join(format!("err_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(base_dir.join("err")).unwrap();

        let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
        layout.create_directories().unwrap();

        let exists_file = layout.file_absolute_path(0).unwrap().to_path_buf();
        let data: Vec<u8> = (0..100u8).collect();
        std::fs::write(&exists_file, &data).unwrap();

        let provider = FileBackedPieceProvider::new(
            base_dir.clone(),
            128,
            1,
            Some(layout),
        );

        let result_valid = provider.get_piece_data(0, 0, 50);
        assert!(result_valid.is_some(), "Read from existing file should succeed");

        let result_oob_piece = provider.get_piece_data(5, 0, 10);
        assert!(result_oob_piece.is_none(), "Out-of-bounds piece index should return None");

        let result_oob_offset = provider.get_piece_data(0, 200, 10);
        assert!(result_oob_offset.is_none() || result_oob_offset.as_ref().map_or(true, |d| d.is_empty()),
            "Out-of-bounds offset should return None or empty");

        assert_eq!(provider.num_pieces(), 1);
        assert!(provider.has_piece(0));

        let result_oob_piece2 = provider.get_piece_data(99, 0, 10);
        assert!(result_oob_piece2.is_none(), "Out-of-bounds piece index should return None");

        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_single_file_provider_with_varying_piece_sizes() {
        let tmp = std::env::temp_dir().join(format!("vary_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("variable.bin");
        let data: Vec<u8> = (0..=999u64).map(|i| (i % 256) as u8).collect();
        std::fs::write(&file_path, &data).unwrap();

        let provider_small = FileBackedPieceProvider::new(file_path.clone(), 256, 4, None);
        assert_eq!(provider_small.num_pieces(), 4);

        let r1 = provider_small.get_piece_data(0, 0, 256);
        assert!(r1.is_some());
        assert_eq!(r1.unwrap().len(), 256);

        let r_last = provider_small.get_piece_data(3, 0, 16);
        assert!(r_last.is_some());
        assert_eq!(r_last.unwrap().len(), 16);

        let provider_large = FileBackedPieceProvider::new(file_path.clone(), 2048, 1, None);
        assert_eq!(provider_large.num_pieces(), 1);

        let r_overflow = provider_large.get_piece_data(0, 900, 200);
        assert!(r_overflow.is_none(), "Read beyond file size should return None");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_bt_command_multiple_peer_management() {
        let mut cmd = create_test_command();

        let config = ChokingConfig {
            max_upload_slots: 2,
            ..Default::default()
        };
        let mut algo = ChokingAlgorithm::new(config);

        for i in 0..5u8 {
            let addr: SocketAddr = format!("10.0.0.{}:6881", i).parse().unwrap();
            let peer = PeerStats::new([i; 20], addr);
            algo.add_peer(peer);
        }
        cmd.choking_algo = Some(algo);

        assert_eq!(cmd.choking_algo.as_ref().unwrap().len(), 5);

        cmd.on_peer_unchoke(0);
        cmd.on_peer_unchoke(2);
        cmd.on_peer_unchoke(4);

        let unchoked_count = cmd.choking_algo.as_ref().unwrap()
            .peers()
            .iter()
            .filter(|p| !p.peer_choking)
            .count();
        assert_eq!(unchoked_count, 3, "Should have 3 unchoked peers");

        for i in 0..5 {
            cmd.on_data_received_from_peer(i, 1024 * (i as u64 + 1));
        }

        let all_active = cmd.choking_algo.as_ref().unwrap()
            .peers()
            .iter()
            .all(|p| !p.is_snubbed);
        assert!(all_active, "All peers should be active after receiving data");
    }

    #[test]
    fn test_bt_command_state_transitions() {
        let mut cmd = create_test_command();

        let config = ChokingConfig::default();
        let algo = ChokingAlgorithm::new(config);
        cmd.choking_algo = Some(algo);

        let addr: SocketAddr = "10.0.0.100:6881".parse().unwrap();
        let idx = cmd.add_peer_to_tracking([0xFF; 8], addr);
        assert_eq!(idx, 0, "First peer should get index 0");

        cmd.on_peer_choke(0);
        cmd.on_peer_unchoke(0);

        assert!(cmd.choking_algo.is_some(), "Command should still have choking algo after peer ops");
    }

    #[test]
    fn test_bt_command_empty_peer_selection() {
        let mut cmd = create_test_command();

        let config = ChokingConfig::default();
        let algo = ChokingAlgorithm::new(config);
        cmd.choking_algo = Some(algo);

        let best = cmd.select_best_peer_for_request();
        assert_eq!(best, None, "Empty peer list should return None");

        let addr: SocketAddr = "10.0.0.200:6881".parse().unwrap();
        let mut peer = PeerStats::new([0xCC; 20], addr);
        peer.peer_choking = true;

        if let Some(ref mut algo) = cmd.choking_algo {
            algo.add_peer(peer);
        }

        let best_after_add = cmd.select_best_peer_for_request();
        assert!(best_after_add.is_some(), "Should return a peer even if all are choked");
    }

    #[test]
    fn test_bt_command_rapid_peer_state_changes() {
        let mut cmd = create_test_command();

        let config = ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        let addr: SocketAddr = "10.0.0.50:6881".parse().unwrap();
        let peer = PeerStats::new([0xDD; 20], addr);
        algo.add_peer(peer);
        cmd.choking_algo = Some(algo);

        for _ in 0..100 {
            cmd.on_peer_unchoke(0);
            cmd.on_peer_choke(0);
        }

        let final_state = cmd.choking_algo.as_ref().unwrap().get_peer(0).unwrap();
        assert!(final_state.peer_choking, "After rapid choke/unchoke cycles, peer should end up choked");
    }
}
