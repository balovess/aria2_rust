//! Unit tests for BtRegistry — pool operations, lookup, and mutation.

use super::super::*;
use std::sync::Arc;

use crate::download::DownloadContext;
use crate::engine::bt_tracker_comm::BtAnnounce;

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
// 1. put and get
// -----------------------------------------------------------------------

#[test]
fn test_put_and_get() {
    let mut registry = BtRegistry::new();
    assert!(registry.get(1).is_none());

    let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
    registry.put(1, obj);

    let retrieved = registry.get(1);
    assert!(retrieved.is_some());
    assert!(retrieved.unwrap().download_context.is_some());
}

// -----------------------------------------------------------------------
// 2. get_mut
// -----------------------------------------------------------------------

#[test]
fn test_get_mut() {
    let mut registry = BtRegistry::new();
    let obj = make_bt_object_with_ctx(1024, 4096, "/tmp/file.bin");
    registry.put(1, obj);

    let obj_mut = registry.get_mut(1).unwrap();
    let ps = Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());
    obj_mut.peer_storage = Some(ps);

    let obj_ref = registry.get(1).unwrap();
    assert!(obj_ref.peer_storage.is_some());
}

#[test]
fn test_get_mut_nonexistent() {
    let mut registry = BtRegistry::new();
    assert!(registry.get_mut(999).is_none());
}

// -----------------------------------------------------------------------
// 3. remove
// -----------------------------------------------------------------------

#[test]
fn test_remove() {
    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
    let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
    registry.put(1, obj1);
    registry.put(2, obj2);

    assert!(registry.remove(1));
    assert!(registry.get(1).is_none());
    assert!(registry.get(2).is_some());

    assert!(!registry.remove(1));
}

// -----------------------------------------------------------------------
// 4. remove_all
// -----------------------------------------------------------------------

#[test]
fn test_remove_all() {
    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
    let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
    registry.put(1, obj1);
    registry.put(2, obj2);

    assert_eq!(registry.len(), 2);
    registry.remove_all();
    assert!(registry.is_empty());
    assert!(registry.get(1).is_none());
    assert!(registry.get(2).is_none());
}

// -----------------------------------------------------------------------
// 5. get_download_context
// -----------------------------------------------------------------------

#[test]
fn test_get_download_context() {
    let mut registry = BtRegistry::new();

    assert!(registry.get_download_context(1).is_none());

    registry.put(1, BtObject::new());
    assert!(registry.get_download_context(1).is_none());

    let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
    let mut obj = BtObject::new();
    obj.download_context = Some(Arc::clone(&ctx));
    registry.put(2, obj);

    let result = registry.get_download_context(2);
    assert!(result.is_some());
    assert!(Arc::ptr_eq(&result.unwrap(), &ctx));
}

// -----------------------------------------------------------------------
// 6. all_download_contexts
// -----------------------------------------------------------------------

#[test]
fn test_all_download_contexts() {
    let mut registry = BtRegistry::new();

    assert!(registry.all_download_contexts().is_empty());

    let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/file1.bin");
    let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/file2.bin");
    registry.put(1, obj1);
    registry.put(2, obj2);

    let contexts = registry.all_download_contexts();
    assert_eq!(contexts.len(), 2);

    registry.put(3, BtObject::new());
    let contexts = registry.all_download_contexts();
    assert_eq!(contexts.len(), 2);
}

// -----------------------------------------------------------------------
// 7. tcp_port / udp_port
// -----------------------------------------------------------------------

#[test]
fn test_tcp_udp_port() {
    let mut registry = BtRegistry::new();

    assert_eq!(registry.tcp_port(), 0);
    assert_eq!(registry.udp_port(), 0);

    registry.set_tcp_port(6881);
    registry.set_udp_port(6882);

    assert_eq!(registry.tcp_port(), 6881);
    assert_eq!(registry.udp_port(), 6882);
}

// -----------------------------------------------------------------------
// 8. Overwrite with put
// -----------------------------------------------------------------------

#[test]
fn test_put_overwrite() {
    let mut registry = BtRegistry::new();

    let obj1 = make_bt_object_with_ctx(1024, 4096, "/tmp/old.bin");
    registry.put(1, obj1);
    assert_eq!(
        registry
            .get(1)
            .unwrap()
            .download_context
            .as_ref()
            .unwrap()
            .get_piece_length(),
        1024
    );

    let obj2 = make_bt_object_with_ctx(2048, 8192, "/tmp/new.bin");
    registry.put(1, obj2);
    assert_eq!(
        registry
            .get(1)
            .unwrap()
            .download_context
            .as_ref()
            .unwrap()
            .get_piece_length(),
        2048
    );
}

// -----------------------------------------------------------------------
// 9. Empty registry operations
// -----------------------------------------------------------------------

#[test]
fn test_empty_registry_operations() {
    let mut registry = BtRegistry::new();

    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.get(1).is_none());
    assert!(registry.get_download_context(1).is_none());
    assert!(registry.all_download_contexts().is_empty());
    assert!(!registry.remove(1));
}

// -----------------------------------------------------------------------
// 10. Builder pattern
// -----------------------------------------------------------------------

#[test]
fn test_bt_object_builder() {
    let ctx = Arc::new(DownloadContext::new(1024, 4096, "/tmp/file.bin".into()));
    let bt_announce = Arc::new(BtAnnounce::new(
        &[],
        &Some("http://tracker.test/announce".to_string()),
    ));
    let peer_storage = Arc::new(crate::engine::bt_peer_storage::DefaultPeerStorage::new());

    let obj = BtObject::builder()
        .download_context(ctx)
        .peer_storage(peer_storage)
        .bt_announce(bt_announce)
        .build();

    assert!(obj.download_context.is_some());
    assert!(obj.piece_storage.is_none());
    assert!(obj.peer_storage.is_some());
    assert!(obj.bt_announce.is_some());
    assert!(obj.bt_progress_manager.is_none());
}

// -----------------------------------------------------------------------
// 11. Default trait
// -----------------------------------------------------------------------

#[test]
fn test_default() {
    let obj = BtObject::default();
    assert!(obj.download_context.is_none());
    assert!(obj.piece_storage.is_none());
    assert!(obj.peer_storage.is_none());
    assert!(obj.bt_announce.is_none());
    assert!(obj.bt_progress_manager.is_none());

    let registry = BtRegistry::default();
    assert!(registry.is_empty());
    assert_eq!(registry.tcp_port(), 0);
    assert_eq!(registry.udp_port(), 0);
    assert!(registry.lpd_message_receiver_id().is_none());
    assert!(registry.udp_tracker_client_id().is_none());
}

// -----------------------------------------------------------------------
// 12. Singleton service IDs
// -----------------------------------------------------------------------

#[test]
fn test_singleton_service_ids() {
    let mut registry = BtRegistry::new();

    assert!(registry.lpd_message_receiver_id().is_none());
    assert!(registry.udp_tracker_client_id().is_none());

    registry.set_lpd_message_receiver_id(100);
    registry.set_udp_tracker_client_id(200);

    assert_eq!(registry.lpd_message_receiver_id(), Some(100));
    assert_eq!(registry.udp_tracker_client_id(), Some(200));
}

// -----------------------------------------------------------------------
// 13. Multiple GIDs in registry
// -----------------------------------------------------------------------

#[test]
fn test_multiple_gids() {
    let mut registry = BtRegistry::new();

    for i in 1..=10 {
        let obj =
            make_bt_object_with_ctx(1024 * i as u32, 4096 * i, &format!("/tmp/file{}.bin", i));
        registry.put(i, obj);
    }

    assert_eq!(registry.len(), 10);
    let contexts = registry.all_download_contexts();
    assert_eq!(contexts.len(), 10);

    for i in (1..=10).step_by(2) {
        assert!(registry.remove(i));
    }

    assert_eq!(registry.len(), 5);
}

// -----------------------------------------------------------------------
// 14. BtObject new() has all None fields
// -----------------------------------------------------------------------

#[test]
fn test_bt_object_new_all_none() {
    let obj = BtObject::new();
    assert!(obj.download_context.is_none());
    assert!(obj.piece_storage.is_none());
    assert!(obj.peer_storage.is_none());
    assert!(obj.bt_announce.is_none());
    assert!(obj.bt_progress_manager.is_none());
}
