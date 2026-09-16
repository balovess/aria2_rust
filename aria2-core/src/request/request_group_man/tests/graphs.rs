use super::*;

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn test_add_metalink_graph_is_metadata_first_and_dependency_gated() {
    let man = RequestGroupMan::new();
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        GroupId::new(42),
        GroupId::new(43),
    )
    .unwrap();
    let (metadata_gid, payload_gid) = man.add_metalink_graph(graph).unwrap();
    assert_eq!(metadata_gid, GroupId::new(42));
    assert_eq!(payload_gid, GroupId::new(43));
    assert_eq!(
        man.reserved.iter_snapshot()[0].recover().gid(),
        metadata_gid
    );
    assert_eq!(man.reserved.iter_snapshot()[1].recover().gid(), payload_gid);
    assert!(
        !man.find_group(payload_gid)
            .unwrap()
            .recover()
            .is_dependency_resolved()
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn test_add_metalink_graph_rejects_duplicate_without_insertion() {
    let man = RequestGroupMan::new();
    man.add_group_with_gid(
        GroupId::new(42),
        vec!["https://example.test/existing".to_string()],
        DownloadOptions::default(),
    )
    .unwrap();
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        GroupId::new(42),
        GroupId::new(43),
    )
    .unwrap();
    assert!(man.add_metalink_graph(graph).is_err());
    assert!(man.find_group(GroupId::new(43)).is_none());
}

#[test]
fn test_add_group_with_gid_preserves_gid_and_advances_allocator() {
    let man = RequestGroupMan::new();
    let explicit_gid = GroupId::new(42);

    man.add_group_with_gid(
        explicit_gid,
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )
    .unwrap();

    assert!(man.find_group(explicit_gid).is_some());
    let generated_gid = man
        .add_group(
            vec!["http://example.com/next.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    assert!(generated_gid.value() > explicit_gid.value());
}

#[test]
fn test_add_restored_group_preserves_gid_and_advances_allocator() {
    let man = RequestGroupMan::new();
    let gid = GroupId::new(0x2a);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec!["http://example.com/restored.bin".to_string()],
        DownloadOptions::default(),
    )));

    man.add_restored_group(group).unwrap();
    assert!(man.find_group(gid).is_some());
    assert!(man.generate_gid().value() > gid.value());
}

#[test]
fn test_add_restored_group_rejects_duplicate_gid() {
    let man = RequestGroupMan::new();
    let gid = GroupId::new(0x2a);
    let group = || {
        Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec!["http://example.com/restored.bin".to_string()],
            DownloadOptions::default(),
        )))
    };

    man.add_restored_group(group()).unwrap();
    assert!(man.add_restored_group(group()).is_err());
}

#[test]
fn test_dependency_blocks_promotion_until_metadata_completes() {
    let man = RequestGroupMan::new();
    let metadata_gid = GroupId::new(10);
    let payload_gid = GroupId::new(11);
    let metadata = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        metadata_gid,
        vec!["https://example.test/file.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let payload = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        payload_gid,
        vec!["bt://payload".to_string()],
        DownloadOptions::default(),
    )));
    payload.recover().set_dependency(Box::new(
        crate::request::request_group::CompletionDependency::new(metadata_gid),
    ));
    payload.recover().set_belongs_to_gid(metadata_gid);

    man.add_restored_group(Arc::clone(&metadata)).unwrap();
    man.add_restored_group(Arc::clone(&payload)).unwrap();

    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].recover().gid(), metadata_gid);
    assert!(man.find_group(payload_gid).is_some());
    assert!(!payload.recover().is_dependency_resolved());

    man.resolve_dependencies_for(metadata_gid);
    let promoted = man.fill_from_reserver();
    assert_eq!(promoted.len(), 1);
    assert_eq!(promoted[0].recover().gid(), payload_gid);
}

#[test]
fn failed_completion_dependency_does_not_leave_reserved_group_stuck() {
    use crate::request::request_group::DownloadResultCode;

    for (prerequisite_status, expected_message) in [
        (
            DownloadStatus::Error("metadata failed".to_string()),
            "completion dependency failed: metadata failed",
        ),
        (DownloadStatus::Removed, "completion dependency was removed"),
    ] {
        let man = RequestGroupMan::new();
        let prerequisite_gid = GroupId::new(60);
        let dependent_gid = GroupId::new(61);
        let prerequisite = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            prerequisite_gid,
            vec!["http://example.com/prerequisite.bin".to_string()],
            DownloadOptions::default(),
        )));
        let dependent = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            dependent_gid,
            vec!["http://example.com/dependent.bin".to_string()],
            DownloadOptions::default(),
        )));
        dependent.recover().set_dependency(Box::new(
            crate::request::request_group::CompletionDependency::new(prerequisite_gid),
        ));
        man.add_restored_group(prerequisite).unwrap();
        man.add_restored_group(dependent).unwrap();

        man.resolve_dependencies_for_status(prerequisite_gid, prerequisite_status);

        assert!(
            man.find_group(dependent_gid).is_none(),
            "failed dependency must leave the canonical group index"
        );
        let result = man
            .find_stopped_result(&dependent_gid.to_hex_string())
            .expect("failed dependency must be recorded as stopped");
        assert_eq!(result.code, DownloadResultCode::UnknownError);
        assert_eq!(
            result.status,
            DownloadStatus::Error(expected_message.to_string())
        );
    }
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn test_failed_metadata_with_direct_fallback_releases_payload() {
    let man = RequestGroupMan::new();
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new_with_fallback(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        GroupId::new(30),
        GroupId::new(31),
        vec!["https://mirror.test/file.bin".to_string()],
    )
    .unwrap();
    man.add_metalink_graph(graph).unwrap();

    man.resolve_dependencies_for_status(
        GroupId::new(30),
        DownloadStatus::Error("metadata unavailable".to_string()),
    );

    let payload = man.find_group(GroupId::new(31)).expect("payload retained");
    assert!(payload.recover().is_dependency_resolved());
    assert_eq!(
        payload
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["https://mirror.test/file.bin"]
    );
}

#[cfg(feature = "metalink")]
#[test]
fn completed_stopped_result_includes_followed_by_child_gids() {
    let man = RequestGroupMan::new();
    let parent_gid = man
        .add_group(
            vec!["https://example.test/index.meta4".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    let parent = man.find_group(parent_gid).expect("parent group");
    parent
        .recover()
        .set_content_type("application/metalink4+xml");
    parent.recover().set_in_memory_data(
            br#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url></file></metalink>"#.to_vec(),
        );

    man.fill_from_reserver();
    parent.recover().mark_complete();

    let demoted = man.remove_stopped_groups(None);

    assert_eq!(demoted, vec![parent_gid]);
    let result = man
        .find_stopped_result(&parent_gid.to_hex_string())
        .expect("completed result must be stored");
    assert_eq!(result.followed_by.len(), 1);
    let child_gid = result.followed_by[0];
    assert!(child_gid != parent_gid);
    assert!(man.find_group(child_gid).is_some());
    assert_eq!(man.reserved.len(), 1);
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn test_failed_torrent_only_metadata_is_stopped_as_error() {
    let man = RequestGroupMan::new();
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        GroupId::new(40),
        GroupId::new(41),
    )
    .unwrap();
    man.add_metalink_graph(graph).unwrap();

    man.resolve_dependencies_for_status(
        GroupId::new(40),
        DownloadStatus::Error("metadata unavailable".to_string()),
    );

    assert!(man.find_group(GroupId::new(41)).is_none());
    assert_eq!(
        man.find_stopped_result(&GroupId::new(41).to_hex_string())
            .map(|result| result.code),
        Some(crate::request::request_group::DownloadResultCode::BittorrentParseError)
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn test_remove_rejects_dependency_blocked_metalink_payload() {
    let man = RequestGroupMan::new();
    let metadata_gid = GroupId::new(50);
    let payload_gid = GroupId::new(51);
    let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
        "https://example.test/file.torrent",
        "file.bin",
        &DownloadOptions::default(),
        metadata_gid,
        payload_gid,
    )
    .unwrap();
    man.add_metalink_graph(graph).unwrap();

    let error = man
        .remove_group(payload_gid)
        .expect_err("an unresolved dependency cannot be removed yet");
    assert!(
        error.to_string().contains("cannot be removed now"),
        "unexpected remove error: {error}"
    );
    let force_error = man
        .force_remove_group(payload_gid)
        .expect_err("force-remove must also respect an unresolved dependency");
    assert!(
        force_error.to_string().contains("cannot be removed now"),
        "unexpected force-remove error: {force_error}"
    );
    assert!(man.find_group(metadata_gid).is_some());
    assert!(man.find_group(payload_gid).is_some());
    assert_eq!(man.stopped_count(), 0);
}

#[test]
fn test_add_group_with_gid_accepts_zero_gid() {
    let man = RequestGroupMan::new();
    let zero_gid = GroupId::new(0);

    man.add_group_with_gid(
        zero_gid,
        vec!["http://example.com/zero.bin".to_string()],
        DownloadOptions::default(),
    )
    .unwrap();

    assert!(man.find_group(zero_gid).is_some());
    let generated_gid = man
        .add_group(
            vec!["http://example.com/next.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
    assert_eq!(generated_gid, GroupId::new(1));
}
