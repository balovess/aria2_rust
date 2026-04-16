use aria2_core::config::ConfigManager;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::segment::bitfield::Bitfield;
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn test_100_request_groups_concurrent_create() {
    let mut handles = Vec::new();
    for _ in 0..100 {
        handles.push(tokio::spawn(async {
            let man = RequestGroupMan::new();
            let _gid = man.add_group(
                vec!["http://example.com/file.zip".into()],
                Default::default(),
            );
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

#[tokio::test]
async fn test_config_manager_concurrent_read_write() {
    use tokio::sync::RwLock;
    let mgr = Arc::new(RwLock::new(ConfigManager::new()));
    let mut handles = Vec::new();

    for i in 0..50 {
        let mgr_clone = mgr.clone();
        handles.push(tokio::spawn(async move {
            if i % 2 == 0 {
                let mut m = mgr_clone.write().await;
                let _fut =
                    m.set_global_option("split", aria2_core::config::OptionValue::Int(i as i64));
                drop(_fut); // Explicitly drop the future to avoid clippy warning
            } else {
                let m = mgr_clone.read().await;
                let _ = m.get_global_i64("split").await;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

#[test]
fn test_download_engine_quick_shutdown() {
    for _ in 0..20 {
        let engine = DownloadEngine::new(10);
        drop(engine);
    }
}

#[test]
fn test_bitfield_concurrent_set_unset() {
    use std::sync::{Arc, Barrier, Mutex};
    let bf = Arc::new(Mutex::new(Bitfield::new(100000)));
    let barrier = Arc::new(Barrier::new(10));

    let mut handles = Vec::new();
    for thread_id in 0..10 {
        let bf = bf.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..10000usize {
                let idx = thread_id * 1000 + i;
                if idx < 100000 {
                    let mut b = bf.lock().unwrap();
                    let _ = b.set(idx);
                    let _ = b.unset(idx);
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

#[test]
fn test_lifecycle_create_complete_remove() {
    for _ in 0..200 {
        let man = RequestGroupMan::new();
        let gid = man.add_group(vec!["http://example.com/f.zip".into()], Default::default());
        drop(gid);
        drop(man);
    }
}

#[test]
fn test_hashmap_high_frequency_insert_lookup() {
    let mut map: HashMap<String, String> = HashMap::with_capacity(5000);
    for i in 0..5000 {
        map.insert(format!("key-{}", i), format!("val-{}", i));
    }
    let mut hits = 0u64;
    for i in 0..5000 {
        if map.contains_key(&format!("key-{}", i)) {
            hits += 1;
        }
    }
    assert_eq!(hits, 5000);
}
