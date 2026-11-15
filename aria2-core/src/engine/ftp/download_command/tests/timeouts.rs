use super::*;

#[test]
fn test_connect_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        connect_timeout: Some(12),
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(102),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(command.connect_timeout, std::time::Duration::from_secs(12));
}

#[test]
fn command_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        timeout: Some(7),
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(103),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(
        Command::timeout(&command),
        Some(Duration::from_secs(7)),
        "timeout must be the configured I/O inactivity duration"
    );
}
