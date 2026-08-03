//! Unit tests for BtRegistry — component accessors, DHT, blocklist, torrent attributes.

use super::super::*;
use std::sync::Arc;

use crate::download::DownloadContext;
use crate::engine::bt_peer_storage::PeerStorage;
use crate::engine::bt_tracker_comm::BtAnnounce;
use crate::segment::piece_storage::PieceStorage;

/// Helper: create a BtObject with DownloadContext and TorrentAttribute.
fn make_bt_object_with_info_hash(
    piece_length: u32,
    total_length: u64,
    path: &str,
    info_hash: &str,
    private: bool,
) -> BtObject {
    use crate::download::download_context::{ContextAttributeType, TorrentAttribute};

    let mut ctx = DownloadContext::new(piece_length, total_length, path.to_string());
    let mut ta = TorrentAttribute::new(info_hash.to_string());
    ta.private_torrent = private;
    ta.name = format!("torrent_{}", info_hash);
    ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
    let ctx = Arc::new(ctx);

    BtObject {
        download_context: Some(ctx),
        ..BtObject::new()
    }
}

/// Helper: create a BtObject with only a DownloadContext.
fn make_bt_object_with_ctx(piece_length: u32, total_length: u64, path: &str) -> BtObject {
    let ctx = Arc::new(DownloadContext::new(
        piece_length,
        total_length,
        path.to_string(),
    ));
    BtObject {
        download_context: Some(ctx),
        ..BtObject::new()
    }
}

// -----------------------------------------------------------------------
// Per-torrent convenience accessors
// -----------------------------------------------------------------------

#[test]
fn test_get_piece_storage() {
    let mut registry = BtRegistry::new();
    let mut obj = BtObject::new();
    let ps: Arc<dyn PieceStorage> = Arc::new(
        crate::segment::piece_storage::DefaultPieceStorage::new(1024, 4096),
    );
    obj.piece_storage = Some(ps);
    registry.put(1, obj);

    assert!(registry.get_piece_storage(1).is_some());
    assert!(registry.get_piece_storage(999).is_none());

    registry.put(2, BtObject::new());
    assert!(registry.get_piece_storage(2).is_none());
}

#[test]
fn test_get_peer_storage() {
    let mut registry = BtRegistry::new();
    let mut obj = BtObject::new();
    let ps: Arc<dyn PeerStorage> =
        Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());
    obj.peer_storage = Some(ps);
    registry.put(1, obj);

    assert!(registry.get_peer_storage(1).is_some());
    assert!(registry.get_peer_storage(999).is_none());
}

#[test]
fn test_get_bt_announce() {
    let mut registry = BtRegistry::new();
    let mut obj = BtObject::new();
    obj.bt_announce = Some(Arc::new(BtAnnounce::new(
        &[],
        &Some("http://tracker.test/announce".to_string()),
    )));
    registry.put(1, obj);

    assert!(registry.get_bt_announce(1).is_some());
    assert!(registry.get_bt_announce(999).is_none());
}

#[test]
fn test_get_piece_storage_by_info_hash() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let mut obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    let ps: Arc<dyn PieceStorage> = Arc::new(
        crate::segment::piece_storage::DefaultPieceStorage::new(1024, 4096),
    );
    obj.piece_storage = Some(ps);

    let mut registry = BtRegistry::new();
    registry.put(1, obj);

    assert!(registry.get_piece_storage_by_info_hash(hash1).is_some());
    assert!(
        registry
            .get_piece_storage_by_info_hash("wrong_hash")
            .is_none()
    );
}

#[test]
fn test_get_peer_storage_by_info_hash() {
    let hash1 = "aaa1111111111111111111111111111111111111";
    let mut obj = make_bt_object_with_info_hash(1024, 4096, "/tmp/a.bin", hash1, false);
    let ps: Arc<dyn PeerStorage> =
        Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());
    obj.peer_storage = Some(ps);

    let mut registry = BtRegistry::new();
    registry.put(1, obj);

    assert!(registry.get_peer_storage_by_info_hash(hash1).is_some());
    assert!(
        registry
            .get_peer_storage_by_info_hash("wrong_hash")
            .is_none()
    );
}

// -----------------------------------------------------------------------
// all_download_finished
// -----------------------------------------------------------------------

#[test]
fn test_all_download_finished_empty_registry() {
    let registry = BtRegistry::new();
    assert!(
        registry.all_download_finished(),
        "empty registry should return true"
    );
}

#[test]
fn test_all_download_finished_no_piece_storage() {
    let mut registry = BtRegistry::new();
    registry.put(1, BtObject::new());
    assert!(!registry.all_download_finished());
}

#[test]
fn test_all_download_finished_with_finished_storage() {
    use crate::segment::piece_storage::DefaultPieceStorage;

    let mut registry = BtRegistry::new();
    let mut obj = BtObject::new();

    let ps: Arc<dyn PieceStorage> = Arc::new(DefaultPieceStorage::new(0, 0));
    obj.piece_storage = Some(ps);
    registry.put(1, obj);

    assert!(registry.all_download_finished());
}

#[test]
fn test_all_download_finished_with_unfinished_storage() {
    use crate::segment::piece_storage::DefaultPieceStorage;

    let mut registry = BtRegistry::new();
    let mut obj = BtObject::new();

    let ps: Arc<dyn PieceStorage> = Arc::new(DefaultPieceStorage::new(1024, 4096));
    obj.piece_storage = Some(ps);
    registry.put(1, obj);

    assert!(!registry.all_download_finished());
}

#[test]
fn test_finished_count() {
    use crate::segment::piece_storage::DefaultPieceStorage;

    let mut registry = BtRegistry::new();

    let mut obj1 = BtObject::new();
    obj1.piece_storage = Some(Arc::new(DefaultPieceStorage::new(0, 0)) as Arc<dyn PieceStorage>);
    registry.put(1, obj1);

    let mut obj2 = BtObject::new();
    obj2.piece_storage =
        Some(Arc::new(DefaultPieceStorage::new(1024, 4096)) as Arc<dyn PieceStorage>);
    registry.put(2, obj2);

    registry.put(3, BtObject::new());

    assert_eq!(registry.finished_count(), 1);
}

// -----------------------------------------------------------------------
// Torrent attribute convenience methods
// -----------------------------------------------------------------------

#[test]
fn test_is_private_torrent() {
    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_info_hash(
        1024,
        4096,
        "/tmp/private.bin",
        "aaa1111111111111111111111111111111111111",
        true,
    );
    registry.put(1, obj1);

    let obj2 = make_bt_object_with_info_hash(
        1024,
        4096,
        "/tmp/public.bin",
        "bbb2222222222222222222222222222222222222",
        false,
    );
    registry.put(2, obj2);

    let obj3 = make_bt_object_with_ctx(1024, 4096, "/tmp/noattr.bin");
    registry.put(3, obj3);

    assert!(registry.is_private_torrent(1));
    assert!(!registry.is_private_torrent(2));
    assert!(!registry.is_private_torrent(3));
    assert!(!registry.is_private_torrent(999));
}

#[test]
fn test_get_torrent_name() {
    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_info_hash(
        1024,
        4096,
        "/tmp/a.bin",
        "aaa1111111111111111111111111111111111111",
        false,
    );
    registry.put(1, obj1);

    let name = registry.get_torrent_name(1);
    assert!(name.is_some());
    assert!(name.unwrap().starts_with("torrent_"));

    let obj2 = make_bt_object_with_ctx(1024, 4096, "/tmp/noattr.bin");
    registry.put(2, obj2);
    assert!(registry.get_torrent_name(2).is_none());

    assert!(registry.get_torrent_name(999).is_none());
}

// -----------------------------------------------------------------------
// DHT engine management
// -----------------------------------------------------------------------

#[test]
fn test_dht_engine_default_none() {
    let registry = BtRegistry::new();
    assert!(registry.get_dht_engine().is_none());
}

#[test]
fn test_dht_engine_set_and_get() {
    use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};

    let rt = tokio::runtime::Runtime::new().unwrap();
    // Local-only config: ephemeral port, no public bootstrap — keeps the
    // test hermetic and instant.
    let config = DhtEngineConfig::local();
    let engine = rt.block_on(async { DhtEngine::start(config).await.unwrap() });

    let mut registry = BtRegistry::new();
    registry.set_dht_engine(engine);

    assert!(registry.get_dht_engine().is_some());
}

#[test]
fn test_dht_engine_clear() {
    use aria2_protocol::bittorrent::dht::engine::{DhtEngine, DhtEngineConfig};

    let rt = tokio::runtime::Runtime::new().unwrap();
    // Local-only config: ephemeral port, no public bootstrap — keeps the
    // test hermetic and instant.
    let config = DhtEngineConfig::local();
    let engine = rt.block_on(async { DhtEngine::start(config).await.unwrap() });

    let mut registry = BtRegistry::new();
    registry.set_dht_engine(engine);
    assert!(registry.get_dht_engine().is_some());

    registry.clear_dht_engine();
    assert!(registry.get_dht_engine().is_none());
}

// -----------------------------------------------------------------------
// Peer blocklist integration
// -----------------------------------------------------------------------

#[test]
fn test_registry_has_empty_blocklist_by_default() {
    let registry = BtRegistry::new();
    assert_eq!(registry.peer_blocklist().count(), 0);
    assert!(!registry.is_peer_blocked("10.0.0.1"));
}

#[test]
fn test_registry_blocklist_add_and_check() {
    let mut registry = BtRegistry::new();
    registry
        .peer_blocklist_mut()
        .add_rule("10.0.0.0/8")
        .unwrap();

    assert!(registry.is_peer_blocked("10.0.0.1"));
    assert!(registry.is_peer_blocked("10.255.255.255"));
    assert!(!registry.is_peer_blocked("192.168.1.1"));
    assert_eq!(registry.peer_blocklist().count(), 1);
}

#[test]
fn test_registry_blocklist_accessors() {
    let mut registry = BtRegistry::new();
    registry
        .peer_blocklist_mut()
        .add_rule("192.168.0.0/16")
        .unwrap();

    assert_eq!(registry.peer_blocklist().count(), 1);
    assert!(registry.peer_blocklist().contains("192.168.1.1"));

    registry.peer_blocklist_mut().clear();
    assert_eq!(registry.peer_blocklist().count(), 0);
}

// -----------------------------------------------------------------------
// Debug output includes new fields
// -----------------------------------------------------------------------

#[test]
fn test_debug_includes_dht_and_index() {
    let registry = BtRegistry::new();
    let debug_str = format!("{:?}", registry);
    assert!(debug_str.contains("has_dht_engine: false"));
    assert!(debug_str.contains("info_hash_index_len: 0"));
}
