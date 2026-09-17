#[cfg(feature = "bittorrent")]
use std::sync::Arc;

use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::MetalinkToRequestGroup;

#[cfg(feature = "metalink")]
#[test]
fn manager_resource_groups_apply_select_file_and_location_priority() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="first.bin"><url location="de" priority="1">https://de.example/first</url></file><file name="second.bin"><url location="us" priority="100">https://us.example/second</url><url location="de" priority="1">https://de.example/second</url></file></metalink>"#;
    let options = DownloadOptions {
        select_file: Some("2".to_string()),
        metalink_location: Some("us".to_string()),
        ..DownloadOptions::default()
    };
    let mut gids = [crate::request::request_group::GroupId::new(70)].into_iter();
    let groups = MetalinkToRequestGroup::new()
        .create_resource_groups_from_bytes(data, &options, &mut gids)
        .expect("filtered Metalink should create one resource group");

    assert_eq!(groups.len(), 1);
    let group = groups[0].recover();
    assert_eq!(group.output_name().as_deref(), Some("second.bin"));
    assert_eq!(
        group
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["https://us.example/second", "https://de.example/second"]
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn create_torrent_graphs_allocates_metadata_and_payload_pairs() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="one.bin"><metaurl mediatype="torrent">https://example.test/one.torrent</metaurl></file><file name="two.bin"><metaurl mediatype="torrent">https://example.test/two.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    let options = DownloadOptions::default();
    let mut gids = [
        crate::request::request_group::GroupId::new(40),
        crate::request::request_group::GroupId::new(41),
        crate::request::request_group::GroupId::new(42),
        crate::request::request_group::GroupId::new(43),
    ]
    .into_iter();
    let graphs = converter
        .create_torrent_graphs_from_bytes(data, &options, &mut gids)
        .expect("torrent-only Metalink should create graphs");
    assert_eq!(graphs.len(), 2);
    assert_eq!(
        graphs[0].metadata.recover().gid(),
        crate::request::request_group::GroupId::new(40)
    );
    assert_eq!(
        graphs[0].payload.recover().gid(),
        crate::request::request_group::GroupId::new(41)
    );
    assert_eq!(
        graphs[1].metadata.recover().gid(),
        crate::request::request_group::GroupId::new(42)
    );
    assert_eq!(
        graphs[1].payload.recover().gid(),
        crate::request::request_group::GroupId::new(43)
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn shared_torrent_metaurl_creates_one_graph_with_all_fallbacks() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="first.bin"><size>10</size><url>https://example.test/first.bin</url><metaurl name="first.bin" mediatype="torrent">https://example.test/shared.torrent</metaurl></file><file name="second.bin"><size>20</size><url>https://example.test/second.bin</url><metaurl name="second.bin" mediatype="torrent">https://example.test/shared.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    let options = DownloadOptions::default();
    let mut gids = [
        crate::request::request_group::GroupId::new(90),
        crate::request::request_group::GroupId::new(91),
    ]
    .into_iter();
    let graphs = converter
        .create_torrent_graphs_from_bytes(data, &options, &mut gids)
        .expect("shared torrent metaurl should create one graph");
    assert_eq!(graphs.len(), 1);
    assert_eq!(
        graphs[0]
            .metadata
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["https://example.test/shared.torrent"]
    );
    assert_eq!(
        graphs[0].payload.recover().gid(),
        crate::request::request_group::GroupId::new(91)
    );
    assert!(!graphs[0].payload.recover().is_dependency_resolved());
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn grouped_metaurl_fixture_has_metadata_payload_and_independent_groups() {
    let data = include_bytes!("../../../../tests/fixtures/grouped_metaurl.xml");
    let converter = MetalinkToRequestGroup::new();
    let options = DownloadOptions::default();
    let mut gids = (1..=6).map(crate::request::request_group::GroupId::new);

    let resource_groups = converter
        .create_resource_groups_from_bytes(data, &options, &mut gids)
        .expect("fixture resources should convert");
    let graphs = converter
        .create_torrent_graphs_from_bytes(data, &options, &mut gids)
        .expect("fixture torrent group should convert");

    assert_eq!(resource_groups.len(), 1);
    assert_eq!(
        resource_groups[0].recover().output_name().as_deref(),
        Some("file2")
    );
    assert_eq!(graphs.len(), 1);
    assert_eq!(
        graphs[0]
            .metadata
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["http://torrent"]
    );
    assert_eq!(
        graphs[0]
            .payload
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["bt://0000000000000002"]
    );

    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let torrent_data = {
        let mut file_entries = Vec::new();
        for name in ["file1", "file3"] {
            let path = BencodeValue::List(vec![BencodeValue::Bytes(name.as_bytes().to_vec())]);
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), BencodeValue::Int(1_000));
            file.insert(b"path".to_vec(), path);
            file_entries.push(BencodeValue::Dict(file));
        }
        let mut info = BTreeMap::new();
        info.insert(b"files".to_vec(), BencodeValue::List(file_entries));
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"bundle".to_vec()));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(2_000));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"https://tracker.test/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));
        BencodeValue::Dict(root).encode()
    };

    let graph = graphs.into_iter().next().expect("shared graph exists");
    let metadata = Arc::clone(&graph.metadata);
    let payload = Arc::clone(&graph.payload);
    metadata.recover().set_in_memory_data(torrent_data);
    let manager = crate::request::request_group_man::RequestGroupMan::new();
    manager
        .add_metalink_graph(graph)
        .expect("graph should be inserted atomically");
    assert_eq!(manager.fill_from_reserver().len(), 1);
    manager.resolve_dependencies_for_status(
        crate::request::request_group::GroupId::new(2),
        crate::request::request_group::DownloadStatus::Complete,
    );
    assert_eq!(manager.fill_from_reserver().len(), 1);
    let context = payload
        .recover()
        .get_download_context()
        .expect("payload context should be resolved");
    let entries = context.get_file_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].original_name(), "file1");
    assert_eq!(entries[1].original_name(), "file3");
    assert_eq!(
        entries[0].remaining_uris().front().map(String::as_str),
        Some("http://file1p1")
    );
    assert_eq!(
        entries[1].remaining_uris().front().map(String::as_str),
        Some("http://file3p1")
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn shared_torrent_graph_maps_torrent_files_to_metalink_paths() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="renamed/one.bin"><size>3</size><url>https://mirror.test/one.bin</url><metaurl name="dir1/file1.txt" mediatype="torrent">https://example.test/shared.torrent</metaurl></file><file name="renamed/two.bin"><size>2</size><url>https://mirror.test/two.bin</url><metaurl name="dir2/file2.dat" mediatype="torrent">https://example.test/shared.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    let options = DownloadOptions::default();
    let mut gids = [
        crate::request::request_group::GroupId::new(100),
        crate::request::request_group::GroupId::new(101),
    ]
    .into_iter();
    let mut graphs = converter
        .create_torrent_graphs_from_bytes(data, &options, &mut gids)
        .expect("shared torrent graph should be created");
    assert_eq!(graphs.len(), 1);

    let torrent_data = {
        let mut file_entries = Vec::new();
        for (length, path) in [
            (3, vec!["dir1", "file1.txt"]),
            (2, vec!["dir2", "file2.dat"]),
        ] {
            let path = BencodeValue::List(
                path.into_iter()
                    .map(|component| BencodeValue::Bytes(component.as_bytes().to_vec()))
                    .collect(),
            );
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), BencodeValue::Int(length));
            file.insert(b"path".to_vec(), path);
            file_entries.push(BencodeValue::Dict(file));
        }
        let mut info = BTreeMap::new();
        info.insert(b"files".to_vec(), BencodeValue::List(file_entries));
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"bundle".to_vec()));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(5));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"https://tracker.test/announce".to_vec()),
        );
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));
        BencodeValue::Dict(root).encode()
    };

    let graph = graphs.pop().unwrap();
    let metadata = Arc::clone(&graph.metadata);
    let payload = Arc::clone(&graph.payload);
    metadata.recover().set_in_memory_data(torrent_data);
    let manager = crate::request::request_group_man::RequestGroupMan::new();
    manager.add_metalink_graph(graph).unwrap();
    assert_eq!(manager.fill_from_reserver().len(), 1);
    manager.resolve_dependencies_for_status(
        crate::request::request_group::GroupId::new(100),
        crate::request::request_group::DownloadStatus::Complete,
    );
    assert_eq!(manager.fill_from_reserver().len(), 1);

    let context = payload
        .recover()
        .get_download_context()
        .expect("resolved payload should have a context");
    let entries = context.get_file_entries();
    assert_eq!(entries.len(), 2);
    assert!(
        entries[0].path().ends_with("renamed\\one.bin")
            || entries[0].path().ends_with("renamed/one.bin")
    );
    assert!(
        entries[1].path().ends_with("renamed\\two.bin")
            || entries[1].path().ends_with("renamed/two.bin")
    );
    assert_eq!(
        entries[0].remaining_uris().front().map(String::as_str),
        Some("https://mirror.test/one.bin")
    );
    assert_eq!(
        entries[1].remaining_uris().front().map(String::as_str),
        Some("https://mirror.test/two.bin")
    );
    assert!(entries.iter().all(|entry| entry.is_requested()));
}

#[cfg(all(feature = "metalink", not(feature = "bittorrent")))]
#[test]
fn mixed_resource_group_is_retained_without_bittorrent_support() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    let mut gids = [crate::request::request_group::GroupId::new(90)].into_iter();
    let groups = converter
        .create_resource_groups_from_bytes(data, &DownloadOptions::default(), &mut gids)
        .expect("mixed Metalink should create one fallback group");
    assert_eq!(groups.len(), 1);
    let source = groups[0]
        .recover()
        .metalink_source()
        .expect("fallback source should be attached");
    assert_eq!(source.1, 0);
    assert_eq!(
        groups[0]
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        vec!["https://example.test/payload.bin"]
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn mixed_resource_and_torrent_entry_uses_graph_fallback() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    let mut gids = [
        crate::request::request_group::GroupId::new(92),
        crate::request::request_group::GroupId::new(93),
    ]
    .into_iter();
    let graphs = converter
        .create_torrent_graphs_from_bytes(data, &DownloadOptions::default(), &mut gids)
        .expect("mixed Metalink should create a graph");
    assert_eq!(graphs.len(), 1);
    assert_eq!(
        graphs[0].metadata.recover().gid(),
        crate::request::request_group::GroupId::new(92)
    );
    assert_eq!(
        graphs[0]
            .payload
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["bt://000000000000005c"]
    );
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn torrent_graph_detects_torrent_metaurl_after_other_metaurl() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://mirror.example/payload.bin</url><metaurl mediatype="xml" priority="1">https://example.test/payload.meta</metaurl><metaurl mediatype="torrent" priority="2">https://example.test/payload.torrent</metaurl></file></metalink>"#;
    let mut gids = [
        crate::request::request_group::GroupId::new(94),
        crate::request::request_group::GroupId::new(95),
    ]
    .into_iter();
    let graphs = MetalinkToRequestGroup::new()
        .create_torrent_graphs_from_bytes(data, &DownloadOptions::default(), &mut gids)
        .expect("torrent metaurl should be detected regardless of position");

    assert_eq!(graphs.len(), 1);
    assert_eq!(
        graphs[0]
            .metadata
            .recover()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["https://example.test/payload.torrent"]
    );
}

#[cfg(feature = "metalink")]
#[test]
fn mixed_resource_and_torrent_entry_is_detected() {
    let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#;
    let converter = MetalinkToRequestGroup::new();
    assert!(
        converter
            .has_mixed_resource_torrent_entries(data, &DownloadOptions::default())
            .expect("Metalink should parse")
    );
}
