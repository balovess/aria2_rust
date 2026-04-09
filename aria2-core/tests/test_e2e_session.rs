use aria2_core::engine::command::Command;
use aria2_core::request::request_group::DownloadOptions;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::session::auto_save_session::AutoSaveSession;
use aria2_core::session::session_serializer::{
    deserialize, load_from_file, save_to_file, serialize_entry, SessionEntry,
};
use std::sync::Arc;
use tokio::sync::RwLock;

#[test]
fn test_e2e_serialize_single_entry() {
    let entry = SessionEntry::new(0xd270c8a2, vec!["http://example.com/file.zip".to_string()]);
    let text = serialize_entry(&entry);
    assert!(text.contains("http://example.com/file.zip"));
    assert!(text.contains("GID=d270c8a2"));

    let entries = deserialize(&text).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].gid, 0xd270c8a2);
}

#[test]
fn test_e2e_serialize_multiple_entries_roundtrip() {
    let mut text = String::new();

    let e1 = SessionEntry::new(1, vec!["http://a.com/1.bin".to_string()]).with_options({
        let mut m = std::collections::HashMap::new();
        m.insert("split".to_string(), "4".to_string());
        m.insert("dir".to_string(), "/tmp".to_string());
        m
    });
    text.push_str(&serialize_entry(&e1));
    text.push('\n');

    let e2 = SessionEntry::new(2, vec!["ftp://b.com/2.iso".to_string()]).paused();
    text.push_str(&serialize_entry(&e2));
    text.push('\n');

    let entries = deserialize(&text).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].uris.len(), 1);
    assert_eq!(entries[0].options.get("split").unwrap(), "4");
    assert!(entries[1].paused);
}

#[test]
fn test_e2e_serialize_special_chars_in_uri() {
    let entry = SessionEntry::new(
        99,
        vec![
            "http://example.com/path?query=foo&bar=baz".to_string(),
            "http://example.com/file with spaces.zip".to_string(),
        ],
    );
    let text = serialize_entry(&entry);

    let entries = deserialize(&text).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].uris.len(), 2);
    assert!(entries[0].uris[0].contains("query=foo&bar=baz"));
}

#[test]
fn test_e2e_deserialize_empty_file() {
    let entries = deserialize("").unwrap();
    assert!(entries.is_empty());

    let entries = deserialize("\n\n\n# comment\n").unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_e2e_deserialize_skip_comments() {
    let input = r#"# Header comment
# Another comment

http://example.com/file
 GID=abc123

# Mid comment
ftp://server/big.iso
 GID=def456
"#;
    let entries = deserialize(input).unwrap();
    assert_eq!(entries.len(), 2);
}

#[test]
fn test_e2e_deserialize_options_parsing() {
    let input = r#"http://example.com/f.zip
 GID=1
 split=4
 max-connection-per-server=2
 dir=C:\Users\test\Downloads
 out=f.zip
"#;
    let entries = deserialize(input).unwrap();
    assert_eq!(entries[0].options.get("split"), Some(&"4".to_string()));
    assert_eq!(
        entries[0].options.get("dir"),
        Some(&"C:\\Users\\test\\Downloads".to_string())
    );
}

#[test]
fn test_e2e_pause_flag_serialization() {
    let input = r#"http://example.com/pause.me
 GID=42
 PAUSE=true
"#;
    let entries = deserialize(input).unwrap();
    assert!(entries[0].paused);

    let text = serialize_entry(&entries[0]);
    assert!(text.contains("PAUSE=true"));
}

#[tokio::test]
async fn test_e2e_atomic_write() {
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    man.write()
        .await
        .add_group(
            vec!["http://example.com/atomic_test.bin".into()],
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let dir = std::env::temp_dir();
    let path = dir.join(format!("e2e_atomic_{}.sess", std::process::id()));

    let groups = man.read().await.list_groups().await;
    save_to_file(&path, &groups).await.unwrap();

    assert!(path.exists());
    let tmp_path = path.with_extension("sess.tmp");
    assert!(!tmp_path.exists());

    let loaded = load_from_file(&path).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].uris[0].contains("atomic_test.bin"));

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn test_e2e_auto_save_interval_logic() {
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let dir = std::env::temp_dir();
    let path = dir.join(format!("e2e_interval_{}.sess", std::process::id()));

    let mut auto = AutoSaveSession::new(path.clone(), std::time::Duration::from_secs(999999), man);
    auto.mark_dirty();

    auto.execute().await.unwrap();
    assert!(!path.exists());

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn test_e2e_auto_save_dirty_flag() {
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let dir = std::env::temp_dir();
    let path = dir.join(format!("e2e_dirty_{}.sess", std::process::id()));

    let mut auto = AutoSaveSession::new(path.clone(), std::time::Duration::from_secs(0), man);

    auto.execute().await.unwrap();
    assert!(!path.exists());

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn test_e2e_full_roundtrip_file_io() {
    let man = Arc::new(RwLock::new(RequestGroupMan::new()));
    man.write()
        .await
        .add_group(
            vec!["http://example.com/roundtrip.bin".into()],
            DownloadOptions {
                split: Some(8),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let dir = std::env::temp_dir();
    let path = dir.join(format!("e2e_roundtrip_{}.sess", std::process::id()));

    let groups = man.read().await.list_groups().await;
    save_to_file(&path, &groups).await.unwrap();

    let loaded = load_from_file(&path).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].uris[0].contains("roundtrip.bin"));
    assert_eq!(loaded[0].options.get("split"), Some(&"8".to_string()));

    let _ = tokio::fs::remove_file(&path).await;
}
