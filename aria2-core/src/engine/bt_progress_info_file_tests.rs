#[cfg(test)]
mod tests {
    use crate::engine::bt_progress_info_file::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// Create a temp directory for testing.
    fn create_test_dir() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % 1_000_000_000;
        let dir = std::env::temp_dir().join(format!("bt_pt_{}_{}", std::process::id(), ts));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("Failed to create test directory");
        dir
    }

    /// Create a BtProgress for testing.
    ///
    /// Uses 4 pieces of 256 KiB each = 1 MiB total.
    /// Bitfield: 1 byte (0xF0 = first 4 bits set).
    fn create_test_progress(info_hash: [u8; 20]) -> BtProgress {
        BtProgress {
            info_hash,
            bitfield: vec![0xF0], // first 4 bits set
            peers: vec![
                PeerAddr { ip: "192.168.1.100".to_string(), port: 6881 },
                PeerAddr { ip: "10.0.0.1".to_string(), port: 6882 },
            ],
            stats: DownloadStats {
                uploaded_bytes: 1024 * 1024 * 100,
                downloaded_bytes: 1024 * 1024 * 500,
                upload_speed: 1024.0 * 50.0,
                download_speed: 1024.0 * 200.0,
                elapsed_seconds: 3600,
            },
            piece_length: 256 * 1024, // 256 KiB
            total_size: 1024 * 1024,  // 1 MiB
            num_pieces: 4,
            upload_length: 1024 * 1024 * 100,
            // 256 KiB piece / 16 KiB block = 16 blocks = 2 bytes bitfield
            in_flight_pieces: vec![
                InFlightPiece::new(3, 256 * 1024, vec![0xC0, 0x00]),
            ],
            is_torrent: true,
            save_time: std::time::SystemTime::now(),
            version: 1,
        }
    }

    #[test]
    fn test_save_load_roundtrip_binary_format() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [
            0xAB, 0xCD, 0xEF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11,
        ];
        let original = create_test_progress(info_hash);

        manager.save_progress(&info_hash, &original).expect("Save failed");
        let loaded = manager.load_progress(&info_hash).expect("Load failed");

        // Verify core fields (binary format persists)
        assert_eq!(loaded.info_hash, original.info_hash);
        assert_eq!(loaded.bitfield, original.bitfield);
        assert_eq!(loaded.piece_length, original.piece_length);
        assert_eq!(loaded.total_size, original.total_size);
        assert_eq!(loaded.upload_length, original.upload_length);
        assert_eq!(loaded.num_pieces, original.num_pieces);
        assert_eq!(loaded.version, original.version);

        // Verify in-flight pieces
        assert_eq!(loaded.in_flight_pieces.len(), original.in_flight_pieces.len());
        for (a, b) in loaded.in_flight_pieces.iter().zip(original.in_flight_pieces.iter()) {
            assert_eq!(a.index, b.index);
            assert_eq!(a.length, b.length);
            assert_eq!(a.bitfield, b.bitfield);
        }

        // Verify file is binary
        let file_path = manager.get_progress_file_path(&info_hash);
        let raw = fs::read(&file_path).expect("Should read file");
        assert_eq!(raw[0], 0x00);
        assert_eq!(raw[1], 0x01);

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_bitfield_serialization_binary() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        // Test with consistent sizes: 4 pieces of 256 KiB = 1 byte bitfield
        let test_cases = [
            (vec![0xFF], "All ones"),    // 8 set bits (but only 4 pieces)
            (vec![0x00], "All zeros"),
            (vec![0xF0], "High 4 bits"),
        ];

        for (i, (bitfield, desc)) in test_cases.iter().enumerate() {
            let mut progress = create_test_progress([i as u8; 20]);
            progress.bitfield = bitfield.clone();

            let hash = [i as u8; 20];
            manager
                .save_progress(&hash, &progress)
                .unwrap_or_else(|_| panic!("Save failed: {}", desc));

            let loaded = manager
                .load_progress(&hash)
                .unwrap_or_else(|_| panic!("Load failed: {}", desc));

            assert_eq!(loaded.bitfield, *bitfield, "Bitfield mismatch: {}", desc);
        }

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_in_flight_pieces_roundtrip() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0xAA; 20];
        let mut progress = create_test_progress(info_hash);
        // 16384 / 16384 = 1 block = 1 byte bitfield
        progress.in_flight_pieces = vec![
            InFlightPiece::new(0, 16384, vec![0xFF]),
            InFlightPiece::new(1, 16384, vec![0xC0]),
        ];

        manager.save_progress(&info_hash, &progress).expect("Save failed");
        let loaded = manager.load_progress(&info_hash).expect("Load failed");

        assert_eq!(loaded.in_flight_pieces.len(), 2);
        assert_eq!(loaded.in_flight_pieces[0].index, 0);
        assert_eq!(loaded.in_flight_pieces[0].length, 16384);
        assert_eq!(loaded.in_flight_pieces[0].bitfield, vec![0xFF]);
        assert_eq!(loaded.in_flight_pieces[1].index, 1);
        assert_eq!(loaded.in_flight_pieces[1].bitfield, vec![0xC0]);

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_sha1_dedup() {
        let test_dir = create_test_dir();
        let mut manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0xBB; 20];
        let progress = create_test_progress(info_hash);

        let written = manager
            .save_progress_with_dedup(&info_hash, &progress)
            .expect("Save failed");
        assert!(written, "First save should write");

        let written = manager
            .save_progress_with_dedup(&info_hash, &progress)
            .expect("Save failed");
        assert!(!written, "Second save should skip (dedup)");

        let mut modified = progress.clone();
        modified.upload_length += 1024;
        let written = manager
            .save_progress_with_dedup(&info_hash, &modified)
            .expect("Save failed");
        assert!(written, "Modified save should write");

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_text_format_backward_compat() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0x22; 20];
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();

        let text_content = format!(
            "[Download]\n\
             info_hash={}\n\
             version=1\n\
             num_pieces=4\n\
             piece_length=262144\n\
             total_size=1048576\n\
             downloaded=524288\n\
             uploaded=262144\n\
             elapsed=60\n\
             bitfield=f0\n\
             [Peers]\n\
             192.168.1.1:6881\n",
            hex_hash
        );

        let file_path = manager.get_progress_file_path(&info_hash);
        fs::write(&file_path, &text_content).expect("Failed to write text file");

        let loaded = manager.load_progress(&info_hash).expect("Load failed");

        assert_eq!(loaded.piece_length, 262144);
        assert_eq!(loaded.total_size, 1048576);
        assert_eq!(loaded.num_pieces, 4);
        assert_eq!(loaded.upload_length, 262144);
        assert_eq!(loaded.bitfield, vec![0xF0]);
        assert_eq!(loaded.peers.len(), 1);

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_atomic_write_safety() {
        let test_dir = create_test_dir();
        let manager = Arc::new(BtProgressManager::new(&test_dir).expect("Failed to create manager"));

        let info_hash = [0x33; 20];
        let num_threads = 5;
        let barrier = Arc::new(Barrier::new(num_threads));
        let mut handles = Vec::with_capacity(num_threads);

        for i in 0..num_threads {
            let manager_clone = Arc::clone(&manager);
            let barrier_clone = Arc::clone(&barrier);

            handles.push(thread::spawn(move || {
                barrier_clone.wait();
                let mut progress = create_test_progress(info_hash);
                progress.upload_length = i as u64 * 1000;
                progress.bitfield = vec![i as u8]; // 1 byte for 4 pieces
                manager_clone
                    .save_progress(&info_hash, &progress)
                    .expect("Concurrent save failed");
            }));
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }

        let loaded = manager
            .load_progress(&info_hash)
            .expect("Load failed after concurrent writes");
        assert_eq!(loaded.info_hash, info_hash);
        assert!(!loaded.bitfield.is_empty());

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_corrupted_file_graceful_degradation() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0x44; 20];
        let file_path = manager.get_progress_file_path(&info_hash);

        // Empty file
        fs::write(&file_path, "").expect("Failed to write empty file");
        let result = manager.load_progress(&info_hash);
        assert!(result.is_err(), "Empty file should return error");

        // Truncated binary
        fs::write(&file_path, [0x00, 0x01, 0x00, 0x00]).expect("Failed to write truncated binary");
        let result = manager.load_progress(&info_hash);
        assert!(result.is_err(), "Truncated binary should return error");

        // Text format with invalid info_hash
        fs::write(
            &file_path,
            "[Download]\ninfo_hash=invalid_hex\nversion=abc\n",
        )
        .expect("Failed to write corrupted file");
        let result = manager.load_progress(&info_hash);
        assert!(result.is_err(), "Corrupted info_hash should return error");

        // Invalid content
        fs::write(&file_path, "This is completely invalid content@@@@###")
            .expect("Failed to write invalid content");
        let result = manager.load_progress(&info_hash);
        assert!(result.is_err() || result.is_ok(), "Should not panic");

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_progress_manager_create_dir() {
        let test_dir = std::env::temp_dir().join("bt_progress_create_dir_test_nested");
        let _ = fs::remove_dir_all(&test_dir);

        let nested_dir = test_dir.join("level1").join("level2").join("level3");
        let manager = BtProgressManager::new(&nested_dir).expect("Failed to create manager");

        assert!(nested_dir.exists());
        assert!(nested_dir.is_dir());

        let info_hash = [0x55; 20];
        let progress = create_test_progress(info_hash);
        manager.save_progress(&info_hash, &progress).expect("Save failed");
        let loaded = manager.load_progress(&info_hash).expect("Load failed");
        assert_eq!(loaded.info_hash, info_hash);

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_remove_progress() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0x66; 20];
        let progress = create_test_progress(info_hash);

        manager.save_progress(&info_hash, &progress).expect("Save failed");
        let file_path = manager.get_progress_file_path(&info_hash);
        assert!(file_path.exists());

        manager.remove_progress(&info_hash).expect("Remove failed");
        assert!(!file_path.exists());

        manager.remove_progress(&info_hash).expect("Repeated removal should succeed");
        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_list_saved_progresses() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        assert!(manager.list_saved_progresses().is_empty());

        let hashes: [[u8; 20]; 3] = [[0x01; 20], [0x02; 20], [0x03; 20]];
        for hash in &hashes {
            let progress = create_test_progress(*hash);
            manager.save_progress(hash, &progress).expect("Save failed");
        }

        let list = manager.list_saved_progresses();
        assert_eq!(list.len(), 3);
        for hash in &hashes {
            assert!(list.contains(hash));
        }

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_completion_ratio_calculation() {
        let progress = BtProgress { num_pieces: 10, bitfield: vec![0x00, 0x00], ..Default::default() };
        assert_eq!(progress.completion_ratio(), 0.0);

        let progress = BtProgress { num_pieces: 8, bitfield: vec![0xFF], ..Default::default() };
        assert!((progress.completion_ratio() - 1.0).abs() < f64::EPSILON);

        let progress = BtProgress { num_pieces: 8, bitfield: vec![0x0F], ..Default::default() };
        assert!((progress.completion_ratio() - 0.5).abs() < 0.01);

        let progress = BtProgress { num_pieces: 10, bitfield: vec![], ..Default::default() };
        assert_eq!(progress.completion_ratio(), 0.0);
    }

    #[test]
    fn test_empty_peers_handling() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0x88; 20];
        let mut progress = create_test_progress(info_hash);
        progress.peers = Vec::new();

        manager.save_progress(&info_hash, &progress).expect("Save failed");
        let loaded = manager.load_progress(&info_hash).expect("Load failed");

        // Binary format does not persist peers
        assert!(loaded.peers.is_empty());
        assert_eq!(loaded.num_pieces, progress.num_pieces);
        assert_eq!(loaded.total_size, progress.total_size);
        assert_eq!(loaded.bitfield, progress.bitfield);

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_info_hash_validation() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0x99; 20];
        let progress = create_test_progress(info_hash);
        manager.save_progress(&info_hash, &progress).expect("Save failed");

        // Loading with a different info_hash tries a non-existent file
        let wrong_hash = [0xFF; 20];
        let result = manager.load_progress(&wrong_hash);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn test_exists() {
        let test_dir = create_test_dir();
        let manager = BtProgressManager::new(&test_dir).expect("Failed to create manager");

        let info_hash = [0xAA; 20];
        assert!(!manager.exists(&info_hash));

        let progress = create_test_progress(info_hash);
        manager.save_progress(&info_hash, &progress).expect("Save failed");
        assert!(manager.exists(&info_hash));

        let _ = fs::remove_dir_all(&test_dir);
    }
}
