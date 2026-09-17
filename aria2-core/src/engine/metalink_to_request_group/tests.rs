use super::*;
use crate::request::request_group::DownloadOptions;

#[cfg(feature = "metalink")]
mod groups;

#[test]
fn options_location_is_applied_case_insensitively() {
    let options = DownloadOptions {
        metalink_location: Some(" US, jp ".to_string()),
        ..DownloadOptions::default()
    };
    let converter = MetalinkToRequestGroup::new();
    let configured = converter
            .generate_from_bytes(
                br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><url location="us" priority="10">http://us/x</url><url location="de" priority="1">http://de/x</url></file></metalink>"#,
                &options,
            )
            .unwrap();
    assert_eq!(configured.len(), 1);

    let doc = MetalinkDocument::parse(
            br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><url location="us" priority="10">http://us/x</url><url location="de" priority="1">http://de/x</url></file></metalink>"#,
            None,
        )
        .unwrap();
    let mut file = doc.files[0].clone();
    file.set_location_priority(&["us"], -LOWEST_PRIORITY);
    assert!(file.urls[0].priority < file.urls[1].priority);
}

fn make_multi_file_metalink() -> Vec<u8> {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="first.bin">
    <size>1024</size>
    <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
    <url priority="1">http://mirror.example.com/first.bin</url>
    <version>1.0</version>
    <language>en</language>
    <os>Linux</os>
  </file>
  <file name="second.bin">
    <size>2048</size>
    <hash type="sha-256">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
    <url priority="1">http://mirror.example.com/second.bin</url>
    <version>2.0</version>
    <language>fr</language>
    <os>Windows</os>
  </file>
</metalink>"#
        .as_bytes()
        .to_vec()
}

#[test]
fn test_generate_from_bytes_no_filter() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new();
    let commands = converter
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .unwrap();
    assert_eq!(commands.len(), 2);
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
#[test]
fn generate_from_bytes_groups_named_shared_torrent_metaurl_files() {
    let data = include_bytes!("../../../tests/fixtures/grouped_metaurl.xml");
    let commands = MetalinkToRequestGroup::new()
        .generate_from_bytes(data, &DownloadOptions::default())
        .expect("grouped Metalink should convert");

    assert_eq!(commands.len(), 2, "file1/file3 share one payload group");
    let grouped = commands
        .iter()
        .find(|command| {
            command
                .group()
                .get_download_context()
                .is_some_and(|context| context.get_file_entries().len() == 2)
        })
        .expect("shared torrent files should form one multi-file payload group");
    let context = grouped
        .group()
        .get_download_context()
        .expect("grouped payload should have a download context");
    let entries = context.get_file_entries();
    assert!(entries[0].path().ends_with("file1"));
    assert_eq!(entries[0].original_name(), "file1");
    assert_eq!(
        entries[0].remaining_uris().front().map(String::as_str),
        Some("http://file1p1")
    );
    assert!(entries[1].path().ends_with("file3"));
    assert_eq!(entries[1].original_name(), "file3");
    assert_eq!(
        entries[1].remaining_uris().front().map(String::as_str),
        Some("http://file3p1")
    );

    let independent = commands
        .iter()
        .find(|command| command.output_path().ends_with("file2"))
        .expect("file2 should remain independent");
    assert_eq!(
        independent
            .group()
            .uris()
            .iter()
            .map(|uri| uri.as_ref())
            .collect::<Vec<_>>(),
        ["http://file2p1"]
    );
}

#[test]
fn test_generate_from_bytes_version_filter() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new().with_version("1.0");
    let commands = converter
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .unwrap();
    assert_eq!(commands.len(), 1);
}

#[test]
fn test_generate_from_bytes_language_filter() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new().with_language("fr");
    let commands = converter
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .unwrap();
    assert_eq!(commands.len(), 1);
}

#[test]
fn test_generate_from_bytes_os_filter() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new().with_os("Linux");
    let commands = converter
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .unwrap();
    assert_eq!(commands.len(), 1);
}

#[test]
fn test_generate_from_bytes_no_match() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new().with_version("99.0");
    let commands = converter
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .unwrap();
    assert!(commands.is_empty());
}

#[test]
fn select_file_option_uses_query_result_positions() {
    let options = DownloadOptions {
        select_file: Some("2".to_string()),
        ..DownloadOptions::default()
    };
    let commands = MetalinkToRequestGroup::new()
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .expect("select-file should be accepted");
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0]
            .output_path()
            .file_name()
            .and_then(|name| name.to_str()),
        Some("second.bin")
    );
}

#[test]
fn select_file_range_keeps_only_requested_entries() {
    let options = DownloadOptions {
        select_file: Some("1-2".to_string()),
        ..DownloadOptions::default()
    };
    let commands = MetalinkToRequestGroup::new()
        .generate_from_bytes(&make_multi_file_metalink(), &options)
        .expect("select-file range should be accepted");
    assert_eq!(commands.len(), 2);
}

#[test]
fn metaurl_only_torrent_entry_is_not_dropped() {
    let options = DownloadOptions::default();
    let converter = MetalinkToRequestGroup::new();
    let commands = converter
            .generate_from_bytes(
                br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload"><metaurl mediatype="torrent">https://example.test/payload.torrent</metaurl></file></metalink>"#,
                &options,
            )
            .unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].group.read().unwrap().uris(),
        Vec::<Box<str>>::new()
    );

    let info = commands[0].file_info.as_ref().unwrap();
    assert_eq!(info.torrent_metaurls.len(), 1);
    assert_eq!(
        info.torrent_metaurls[0].url,
        "https://example.test/payload.torrent"
    );
}

#[test]
fn pause_requested_marks_generated_commands() {
    let commands = MetalinkToRequestGroup::new()
            .with_pause_requested(true)
            .generate_from_bytes(
                br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload"><url>https://example.test/payload</url></file></metalink>"#,
                &DownloadOptions::default(),
            )
            .expect("Metalink should generate a command");

    assert_eq!(commands.len(), 1);
    assert!(commands[0].group().is_pause_requested());
}

#[test]
fn test_default() {
    let _converter = MetalinkToRequestGroup::default();
}
