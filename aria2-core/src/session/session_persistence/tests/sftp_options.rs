use super::*;

#[tokio::test]
async fn restored_sftp_options_use_the_shared_option_parser() {
    let session_dir = create_test_session_dir();
    let resume_data = ResumeData {
        gid: "babe".to_string(),
        uris: vec![crate::engine::resume_data::UriState {
            uri: "sftp://user@example.test/file".to_string(),
            ..Default::default()
        }],
        status: "paused".to_string(),
        options: [
            ("ssh-host-key-md".to_string(), "sha-1=abcd".to_string()),
            ("sftp-connect-timeout".to_string(), "0".to_string()),
            ("sftp-read-timeout".to_string(), "41".to_string()),
            ("sftp-compression".to_string(), "true".to_string()),
            ("sftp-keepalive-interval".to_string(), "0".to_string()),
            (
                "sftp-preferred-ciphers".to_string(),
                "cipher-a,cipher-b".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    resume_data
        .save_to_file(&session_dir.join("babe.aria2"))
        .expect("resume data should save");

    let mut persistence = SessionPersistence::new(&session_dir);
    let mut groups = Vec::new();
    assert_eq!(persistence.load_state(&mut groups).await.unwrap(), 1);

    let restored = groups[0].recover();
    let options = restored.options();
    assert_eq!(options.ssh_host_key_md.as_deref(), Some("sha-1=abcd"));
    assert_eq!(options.sftp_connect_timeout, None);
    assert_eq!(options.sftp_read_timeout, Some(41));
    assert!(options.sftp_compression);
    assert_eq!(options.sftp_keepalive_interval, Some(0));
    assert_eq!(
        options.sftp_preferred_ciphers.as_deref(),
        Some(["cipher-a".to_string(), "cipher-b".to_string()].as_slice())
    );

    fs::remove_dir_all(&session_dir).expect("test session directory should be removed");
}
