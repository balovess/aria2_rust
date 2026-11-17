use super::*;

#[tokio::test]
async fn retry_wait_is_interruptible_when_paused() {
    let command = FtpDownloadCommand::new(
        GroupId::new(104),
        "ftp://example.com/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    command.group.write().unwrap().pause().unwrap();

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        command.wait_for_retry(Duration::from_secs(5)),
    )
    .await
    .expect("paused retry wait should stop promptly");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test]
async fn retry_wait_wakes_when_paused_after_wait_starts() {
    let command = FtpDownloadCommand::new(
        GroupId::new(105),
        "ftp://example.com/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    let group = std::sync::Arc::clone(&command.group);
    let wait_task =
        tokio::spawn(async move { command.wait_for_retry(Duration::from_secs(5)).await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    group.write().unwrap().pause().unwrap();

    let result = tokio::time::timeout(Duration::from_millis(100), wait_task)
        .await
        .expect("pause should wake an active FTP retry wait")
        .expect("FTP retry wait task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}
