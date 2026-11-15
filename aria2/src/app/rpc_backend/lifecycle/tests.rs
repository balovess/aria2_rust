use super::*;
use aria2_core::engine::engine_command::channel;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::request::request_group_man::RequestGroupMan;
use std::sync::Arc;
use tokio::sync::RwLock;

fn backend_with_group() -> (CoreRpcBackend, Arc<RequestGroupMan>, GroupId) {
    let group_man = Arc::new(RequestGroupMan::new());
    let gid = group_man
        .add_group(
            vec!["https://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("group should be added");
    let (sender, _) = channel();
    let backend = CoreRpcBackend::new(
        Arc::clone(&group_man),
        sender,
        Arc::new(RwLock::new(aria2_core::config::ConfigManager::new())),
        None,
        "test",
    );
    (backend, group_man, gid)
}

#[test]
fn change_option_updates_task_options_through_group_lifecycle() {
    let (backend, group_man, gid) = backend_with_group();

    backend
        .change_option(
            gid.to_hex_string(),
            HashMap::from([
                (
                    "max-download-limit".to_string(),
                    serde_json::json!(["64K", 123, false]),
                ),
                ("max-upload-limit".to_string(), serde_json::json!(123)),
                ("unrecognized-extension".to_string(), serde_json::json!("x")),
            ]),
        )
        .expect("valid option change should succeed");

    let group = group_man.find_group(gid).expect("group remains registered");
    let group = group.read().expect("group lock should not be poisoned");
    assert_eq!(
        group.runtime_options().get("max-download-limit"),
        Some(&serde_json::json!("64K"))
    );
    assert!(
        !group
            .runtime_options()
            .contains_key("unrecognized-extension")
    );
    assert!(!group.runtime_options().contains_key("max-upload-limit"));
}

#[test]
fn change_option_rejects_invalid_known_value_without_partial_update() {
    let (backend, group_man, gid) = backend_with_group();

    let result = backend.change_option(
        gid.to_hex_string(),
        HashMap::from([
            (
                "max-download-limit".to_string(),
                serde_json::json!("not-a-rate"),
            ),
            ("max-upload-limit".to_string(), serde_json::json!("32K")),
        ]),
    );

    assert!(matches!(result, Err(BackendError::Execution(_))));
    let group = group_man.find_group(gid).expect("group remains registered");
    assert!(
        group
            .read()
            .expect("group lock should not be poisoned")
            .runtime_options()
            .is_empty()
    );
}
