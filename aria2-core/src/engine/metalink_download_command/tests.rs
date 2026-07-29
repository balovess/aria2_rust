use super::*;
use crate::request::request_group::{DownloadOptions, GroupId};
use aria2_protocol::metalink::parser::UrlEntry;

#[test]
fn test_priority_ascending_order() {
    let urls = vec![
        UrlEntry::new("http://mirror3.example.com/file.bin").with_priority(3),
        UrlEntry::new("http://mirror1.example.com/file.bin").with_priority(1),
        UrlEntry::new("http://mirror2.example.com/file.bin").with_priority(2),
    ];

    let sorted = select_mirrors_by_priority(&urls, "");

    assert_eq!(sorted.len(), 3);
    assert_eq!(sorted[0].priority, 1);
    assert_eq!(sorted[1].priority, 2);
    assert_eq!(sorted[2].priority, 3);
}

#[test]
fn test_location_preference_boosts_matching() {
    let urls = vec![
        UrlEntry::new("http://us-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("us"),
        UrlEntry::new("http://eu-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("eu"),
        UrlEntry::new("http://eu-mirror2.example.com/file.bin")
            .with_priority(5)
            .with_location("eu"),
        UrlEntry::new("http://jp-mirror1.example.com/file.bin")
            .with_priority(5)
            .with_location("jp"),
    ];

    let sorted = select_mirrors_by_priority(&urls, "eu");

    assert_eq!(sorted.len(), 4);

    // EU mirrors should appear before non-EU mirrors
    let first_non_eu_idx = sorted
        .iter()
        .position(|u| u.location.as_deref() != Some("eu"))
        .expect("Should find at least one non-EU mirror");

    let last_eu_idx = sorted
        .iter()
        .rposition(|u| u.location.as_deref() == Some("eu"))
        .expect("Should find EU mirrors");

    assert!(last_eu_idx < first_non_eu_idx);
}

#[tokio::test]
async fn test_failover_tries_all_then_errors() {
    let urls = [
        UrlEntry::new("http://mirror1.fail/file.bin").with_priority(3),
        UrlEntry::new("http://mirror2.fail/file.bin").with_priority(2),
        UrlEntry::new("http://mirror3.fail/file.bin").with_priority(1),
    ];

    let fail_fn = |url: &str| -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Vec<u8>, String>> + '_>,
    > {
        let url_owned = url.to_string();
        Box::pin(async move { Err(format!("Connection refused to {}", url_owned)) })
    };

    let url_refs: Vec<&UrlEntry> = urls.iter().collect();
    let result = try_mirrors_with_failover(&url_refs, fail_fn).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("All 3 mirrors failed"));
}

#[tokio::test]
async fn test_single_mirror_no_failover_needed() {
    let urls = [UrlEntry::new("http://working-mirror.example.com/success.bin").with_priority(10)];

    let expected_data = b"Downloaded file content".to_vec();
    let data_shared = std::sync::Arc::new(expected_data.clone());
    let success_fn = move |_url: &str| {
        let data = data_shared.clone();
        async move { Ok((*data).clone()) }
    };

    let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &success_fn).await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), expected_data);
}

#[test]
fn test_priority_overrides_location() {
    let urls = vec![
        UrlEntry::new("http://high-eu.example.com/file.bin")
            .with_priority(10)
            .with_location("eu"),
        UrlEntry::new("http://low-us.example.com/file.bin")
            .with_priority(1)
            .with_location("us"),
    ];

    let sorted = select_mirrors_by_priority(&urls, "eu");
    assert_eq!(sorted[0].priority, 1);
    assert_eq!(sorted[1].priority, 10);
}

#[test]
fn test_empty_resources_returns_empty() {
    let urls: Vec<UrlEntry> = Vec::new();
    let sorted = select_mirrors_by_priority(&urls, "");
    assert!(sorted.is_empty());
}

#[tokio::test]
async fn test_failover_succeeds_on_second_mirror() {
    let urls = [
        UrlEntry::new("http://failing-mirror.example.com/file.bin").with_priority(1),
        UrlEntry::new("http://working-mirror.example.com/file.bin").with_priority(2),
    ];

    let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_clone = attempt_count.clone();
    let fallback_fn = move |url: &str| {
        let url_owned = url.to_string();
        let count = count_clone.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if url_owned.contains("failing") {
                Err("Connection timeout".to_string())
            } else {
                Ok(b"Success data".to_vec())
            }
        }
    };

    let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &fallback_fn).await;

    assert!(result.is_ok());
    assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(result.unwrap(), b"Success data");
}

// ── Multi-file Metalink tests ─────────────────────────────────────────

fn make_multi_file_xml() -> Vec<u8> {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="first.bin">
      <size>1024</size>
      <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
      <url priority="1">http://mirror.example.com/first.bin</url>
    </file>
    <file name="second.bin">
      <size>2048</size>
      <hash type="sha-256">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
      <url priority="1">http://mirror.example.com/second.bin</url>
    </file>
  </files>
</metalink>"#
        .as_bytes()
        .to_vec()
}

#[test]
fn test_new_accepts_multi_file_metalink() {
    let options = DownloadOptions::default();
    // Previously this would return "Metalink contains multiple files or no files"
    // Now it should succeed, picking the first file
    let result = MetalinkDownloadCommand::new(GroupId::new(1), &make_multi_file_xml(), &options, None);
    assert!(result.is_ok(), "new() should accept multi-file Metalink");
}

#[test]
fn test_create_multi_file_returns_all_files() {
    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 100)
            .unwrap();

    assert_eq!(commands.len(), 2, "Should create 2 commands for 2 files");
    assert_eq!(commands[0].file_index, 0);
    assert_eq!(commands[1].file_index, 1);
    assert!(
        commands[0]
            .command
            .output_path
            .to_string_lossy()
            .contains("first.bin"),
        "First command should be for first.bin"
    );
    assert!(
        commands[1]
            .command
            .output_path
            .to_string_lossy()
            .contains("second.bin"),
        "Second command should be for second.bin"
    );
}

#[test]
fn test_create_multi_file_assigns_incrementing_gids() {
    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 200)
            .unwrap();

    let g0 = commands[0].command.group.read().unwrap();
    let g1 = commands[1].command.group.read().unwrap();
    assert_eq!(g0.gid().value(), 200);
    assert_eq!(g1.gid().value(), 201);
}

#[test]
fn test_create_multi_file_skips_files_without_urls() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="with-urls.bin">
      <size>1024</size>
      <url priority="1">http://mirror.example.com/with-urls.bin</url>
    </file>
    <file name="no-urls.bin">
      <size>2048</size>
    </file>
  </files>
</metalink>"#;

    let options = DownloadOptions::default();
    let commands =
        MetalinkDownloadCommand::create_multi_file(xml.as_bytes(), &options, None, 1).unwrap();

    assert_eq!(commands.len(), 1, "Should skip file with no URLs");
    assert_eq!(commands[0].file_index, 0);
}

#[test]
fn test_output_path_accessor() {
    let options = DownloadOptions::default();
    let commands = MetalinkDownloadCommand::create_multi_file(
        &make_multi_file_xml(),
        &options,
        Some("/tmp"),
        1,
    )
    .unwrap();

    assert!(
        commands[0]
            .command
            .output_path()
            .to_string_lossy()
            .contains("first.bin")
    );
}
