use aria2_core::config::netrc::NetRcFile;
use aria2_core::config::parser::ConfigParser;
use aria2_core::config::uri_list::UriListFile;
use aria2_core::config::{ConfigManager, OptionValue};
use std::fs;

fn create_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录失败")
}
fn write_file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    fs::write(&path, content).expect("写入临时文件失败");
    path.to_string_lossy().to_string()
}

#[test]
fn test_config_loading_priority() {
    let tmp = create_temp_dir();
    let conf_path = write_file(&tmp, "aria2.conf", "split=3\nquiet=true\n");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        unsafe { std::env::set_var("ARIA2_SPLIT", "5") };

        mgr.load_file(&conf_path).await;
        mgr.load_env().await;
        mgr.set_global_option("split", OptionValue::Int(10))
            .await
            .unwrap();

        let val = mgr.get_global_i64("split").await;
        assert_eq!(val, Some(10));

        unsafe { std::env::remove_var("ARIA2_SPLIT") };
    });
}

#[test]
fn test_config_unknown_option_rejected() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        let result = mgr
            .set_global_option("nonexistent-option-xyz", OptionValue::Str("value".into()))
            .await;
        assert!(result.is_err());
    });
}

#[test]
fn test_config_boolean_inversion() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("check-certificate", OptionValue::Bool(false))
            .await
            .unwrap();
        let val = mgr.get_global_bool("check-certificate").await;
        assert_eq!(val, Some(false));
    });
}

#[test]
fn test_config_size_parsing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("piece-length", OptionValue::Str("16M".into()))
            .await
            .unwrap();
        let val = mgr.get_global_i64("piece-length").await;
        assert_eq!(val, Some(16 * 1024 * 1024));
    });
}

#[test]
fn test_task_options_inherit_and_override() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        mgr.set_global_option("split", OptionValue::Int(5))
            .await
            .unwrap();
        mgr.set_task_option("gid-001", "split", OptionValue::Int(12))
            .await
            .unwrap();

        let global_split = mgr.get_global_i64("split").await;
        let task_split = mgr.get_task_default("gid-001", "split").await;
        let other_task_split = mgr.get_task_default("gid-002", "split").await;

        assert_eq!(global_split, Some(5));
        assert_eq!(task_split.and_then(|v| v.as_i64()), Some(12));
        assert_eq!(other_task_split.and_then(|v| v.as_i64()), Some(5));
    });
}

#[test]
fn test_uri_list_file_parse() {
    let tmp = create_temp_dir();
    let content = "http://example.com/a.zip\nhttp://example.com/b.iso\thttp://mirror.com/b.iso\n";
    let path = write_file(&tmp, "uris.txt", content);

    let file = UriListFile::from_file(&path).unwrap();
    assert_eq!(file.len(), 2);
    assert_eq!(file.entries()[0].uris.len(), 1);
    assert_eq!(file.entries()[1].uris.len(), 2);
}

#[test]
fn test_uri_list_with_options() {
    let tmp = create_temp_dir();
    let content = r#"  dir=/downloads
  out=bigfile.bin
http://example.com/large.bin
"#;
    let path = write_file(&tmp, "opts.txt", content);

    let file = UriListFile::from_file(&path).unwrap();
    assert_eq!(file.len(), 1);
    assert_eq!(
        file.entries()[0].option("dir").map(|s| s.as_str()),
        Some("/downloads")
    );
}

#[test]
fn test_netrc_parse_and_find() {
    let tmp = create_temp_dir();
    let content = "machine ftp.example.com\nlogin myuser\npassword mypass\ndefault\nlogin anon\npassword guest@\n";
    let path = write_file(&tmp, ".netrc", content);

    let netrc = NetRcFile::from_file(&path).unwrap();
    let creds = netrc.get_credentials("ftp.example.com");
    assert!(creds.is_some());
    let (user, pass) = creds.unwrap();
    assert_eq!(user, "myuser");
    assert_eq!(pass, "mypass");

    let default_creds = netrc.get_credentials("unknown.host.com");
    assert!(default_creds.is_some());
    let (du, _) = default_creds.unwrap();
    assert_eq!(du, "anon");
}

#[test]
fn test_session_save_load_roundtrip() {
    let tmp = create_temp_dir();
    let session_path = tmp.path().join("session.txt").to_string_lossy().to_string();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr1 = ConfigManager::new();
        mgr1.set_global_option("split", OptionValue::Int(7))
            .await
            .unwrap();
        mgr1.set_global_option("dir", OptionValue::Str("/downloads".into()))
            .await
            .unwrap();
        mgr1.save_session(&session_path).await.unwrap();

        let mut mgr2 = ConfigManager::new();
        mgr2.load_session(&session_path).await.unwrap();

        assert_eq!(mgr2.get_global_i64("split").await, Some(7));
        assert_eq!(mgr2.get_global_str("dir").await, Some("/downloads".into()));
    });
}

#[test]
fn test_change_event_broadcast() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new();
        let mut rx = mgr.subscribe_changes();
        mgr.set_global_option("quiet", OptionValue::Bool(true))
            .await
            .unwrap();

        let event = rx.recv().await;
        assert!(event.is_ok());
        let evt = event.unwrap();
        assert_eq!(evt.key, "quiet");
    });
}

#[test]
fn test_parser_cli_args() {
    let registry = aria2_core::config::OptionRegistry::new();
    let parser = ConfigParser::with_registry(registry);
    let args: Vec<&str> = vec![
        "--dir=/my/dir",
        "--split=8",
        "--quiet",
        "--out=output.dat",
        "http://example.com/file.zip",
    ];
    let mut p = parser;
    p.parse_cli_args(&args);

    assert_eq!(p.get_str("dir"), Some("/my/dir"));
    assert_eq!(p.get_i64("split"), Some(8));
    assert_eq!(p.get_bool("quiet"), Some(true));
    assert_eq!(p.get_str("out"), Some("output.dat"));
}

#[test]
fn test_registry_has_common_options() {
    let reg = aria2_core::config::OptionRegistry::new();
    assert!(reg.contains("dir"));
    assert!(reg.contains("split"));
    assert!(reg.contains("out"));
    assert!(reg.contains("max-download-limit"));
    assert!(reg.contains("timeout"));
    assert!(reg.contains("max-tries"));
    assert!(!reg.contains("nonexistent"));
}

#[test]
fn test_all_global_options_json_output() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mgr = ConfigManager::new();
        let json = mgr.get_all_global_options_json().await;
        assert!(json.is_object());
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("dir"));
        assert!(obj.contains_key("split"));
    });
}
