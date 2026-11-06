use super::super::*;
use crate::metalink::resource::ResourceType;

// ========================================================================
// MetalinkFile method tests
// ========================================================================

#[test]
fn test_drop_unsupported_resources() {
    let mut file = MetalinkFile::new("test.bin");
    file.urls
        .push(UrlEntry::new("http://a.com/f").with_resource_type(ResourceType::Http));
    file.urls
        .push(UrlEntry::new("https://b.com/f").with_resource_type(ResourceType::Https));
    file.urls
        .push(UrlEntry::new("ftp://c.com/f").with_resource_type(ResourceType::Ftp));
    file.urls.push(
        UrlEntry::new("magnet:?xt=urn:btih:abc").with_resource_type(ResourceType::BitTorrent),
    );
    file.urls
        .push(UrlEntry::new("http://d.com/f").with_resource_type(ResourceType::NotSupported));
    file.urls
        .push(UrlEntry::new("http://e.com/f").with_resource_type(ResourceType::Unknown));
    file.drop_unsupported_resources();
    // Both NotSupported and Unknown are removed, matching C++ default case
    assert_eq!(file.urls.len(), 4);
    assert!(file.urls.iter().all(|u| u.is_supported()));
}

#[test]
fn test_set_location_priority() {
    let mut file = MetalinkFile::new("test.bin");
    file.urls.push(
        UrlEntry::new("http://a.com/f")
            .with_location("cn")
            .with_priority(10),
    );
    file.urls.push(
        UrlEntry::new("http://b.com/f")
            .with_location("us")
            .with_priority(10),
    );
    file.urls.push(
        UrlEntry::new("http://c.com/f")
            .with_location("jp")
            .with_priority(10),
    );
    file.urls.push(UrlEntry::new("http://d.com/f")); // no location

    // Boost cn and jp locations by -999999 (mirrors C++ usage)
    file.set_location_priority(&["cn", "jp"], -999999);

    assert_eq!(file.urls[0].priority, 10 - 999999);
    assert_eq!(file.urls[1].priority, 10); // us: unchanged
    assert_eq!(file.urls[2].priority, 10 - 999999);
    assert_eq!(file.urls[3].priority, 999999); // no location: unchanged
}

#[test]
fn test_set_protocol_priority() {
    let mut file = MetalinkFile::new("test.bin");
    file.urls.push(
        UrlEntry::new("http://a.com/f")
            .with_resource_type(ResourceType::Http)
            .with_priority(10),
    );
    file.urls.push(
        UrlEntry::new("https://b.com/f")
            .with_resource_type(ResourceType::Https)
            .with_priority(10),
    );
    file.urls.push(
        UrlEntry::new("ftp://c.com/f")
            .with_resource_type(ResourceType::Ftp)
            .with_priority(10),
    );

    // Boost https by -999999 (mirrors C++ usage with preferred protocol)
    file.set_protocol_priority("https", -999999);

    assert_eq!(file.urls[0].priority, 10);
    assert_eq!(file.urls[1].priority, 10 - 999999);
    assert_eq!(file.urls[2].priority, 10);
}

#[test]
fn test_reorder_resources_by_priority() {
    let mut file = MetalinkFile::new("test.bin");
    file.urls
        .push(UrlEntry::new("http://a.com/f").with_priority(30));
    file.urls
        .push(UrlEntry::new("http://b.com/f").with_priority(10));
    file.urls
        .push(UrlEntry::new("http://c.com/f").with_priority(20));

    file.reorder_resources_by_priority();

    // After shuffle+sort, must be in ascending priority order
    assert_eq!(file.urls[0].priority, 10);
    assert_eq!(file.urls[1].priority, 20);
    assert_eq!(file.urls[2].priority, 30);
}

#[test]
fn test_reorder_metaurls_by_priority() {
    let mut file = MetalinkFile::new("test.bin");
    file.meta_urls
        .push(MetaUrlEntry::new("http://a.com/torrent", MediaType::Torrent).with_priority(30));
    file.meta_urls
        .push(MetaUrlEntry::new("http://b.com/torrent", MediaType::Torrent).with_priority(10));
    file.meta_urls
        .push(MetaUrlEntry::new("http://c.com/torrent", MediaType::Torrent).with_priority(20));

    file.reorder_metaurls_by_priority();

    assert_eq!(file.meta_urls[0].priority, 10);
    assert_eq!(file.meta_urls[1].priority, 20);
    assert_eq!(file.meta_urls[2].priority, 30);
}

// ========================================================================
// group_entry_by_metaurl_name tests
// ========================================================================

#[test]
fn test_group_entry_no_metaurls() {
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size = Some(100);
    f1.size_known = true;
    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // No metaurls → each gets its own group with empty key
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].0, "");
    assert_eq!(groups[0].1, vec![0]);
    assert_eq!(groups[1].0, "");
    assert_eq!(groups[1].1, vec![1]);
}

#[test]
fn test_group_entry_same_metaurl_merges() {
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size = Some(100);
    f1.size_known = true;
    f1.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("a.bin")
        .with_priority(1),
    );

    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;
    f2.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("b.bin")
        .with_priority(1),
    );

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // Same metaurl URL, both have names, both size_known → merged
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].0, "http://torrent.example.com/meta.torrent");
    assert_eq!(groups[0].1, vec![0, 1]);
}

#[test]
fn test_group_entry_empty_name_no_merge() {
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size = Some(100);
    f1.size_known = true;
    f1.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_priority(1),
        // name is None
    );

    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;
    f2.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("b.bin")
        .with_priority(1),
    );

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // f1 has no name → cannot merge; f2 gets its own group
    assert_eq!(groups.len(), 2);
}

#[test]
fn test_group_entry_size_unknown_no_merge() {
    // When an entry has size_unknown, it cannot initiate a merge search,
    // but other entries with size_known can still merge into it.
    // This mirrors C++ where !entry->sizeKnown just skips the search loop
    // for that entry, but the group's first entry only needs a non-empty name
    // for others to merge into.
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size_known = false; // size unknown
    f1.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("a.bin")
        .with_priority(1),
    );

    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;
    f2.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("b.bin")
        .with_priority(1),
    );

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // f1 size unknown → cannot search for merge → creates new group
    // f2 size known → searches, finds f1's group (same URL, f1 has name) → merges
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].1, vec![0, 1]);
}

#[test]
fn test_group_entry_size_unknown_and_no_name() {
    // When the first entry in a group has no name, subsequent entries
    // cannot merge into it (group_first_has_name check fails).
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size_known = false;
    f1.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_priority(1), // no name
    );

    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;
    f2.meta_urls.push(
        MetaUrlEntry::new(
            "http://torrent.example.com/meta.torrent",
            MediaType::Torrent,
        )
        .with_name("b.bin")
        .with_priority(1),
    );

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // f1 has no name → f2 cannot merge into it (group_first_has_name is false)
    assert_eq!(groups.len(), 2);
}

#[test]
fn test_group_entry_different_metaurl_no_merge() {
    let mut f1 = MetalinkFile::new("a.bin");
    f1.size = Some(100);
    f1.size_known = true;
    f1.meta_urls.push(
        MetaUrlEntry::new("http://a.com/meta.torrent", MediaType::Torrent)
            .with_name("a.bin")
            .with_priority(1),
    );

    let mut f2 = MetalinkFile::new("b.bin");
    f2.size = Some(200);
    f2.size_known = true;
    f2.meta_urls.push(
        MetaUrlEntry::new("http://b.com/meta.torrent", MediaType::Torrent)
            .with_name("b.bin")
            .with_priority(1),
    );

    let groups = group_entry_by_metaurl_name(&[f1, f2]);
    // Different metaurl URLs → separate groups
    assert_eq!(groups.len(), 2);
}
