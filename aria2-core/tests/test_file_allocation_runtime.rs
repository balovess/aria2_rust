use aria2_core::filesystem::file_allocation::AllocationStrategy;
use aria2_core::filesystem::file_allocation_man::{enqueue_path, shared};
use std::time::Duration;

#[test]
fn shared_worker_is_not_bound_to_its_creator_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocated.bin");

    let first_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    first_runtime.block_on(async {
        shared();
        tokio::task::yield_now().await;
    });

    let second_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    second_runtime.block_on(async {
        let manager = shared();
        tokio::time::timeout(
            Duration::from_secs(2),
            enqueue_path(&manager, &path, 4096, AllocationStrategy::Trunc, false, 1),
        )
        .await
        .expect("allocation worker must outlive the creator runtime")
        .expect("allocation must succeed");
    });

    assert_eq!(std::fs::metadata(path).unwrap().len(), 4096);
}
