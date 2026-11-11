use std::sync::Arc;
use std::time::Duration;

use crate::engine::download_progress::ProgressUpdater;
use crate::engine::http::cookie_helper::CookieHelper;
use crate::http::HttpRequestPolicy;
use crate::http::cookie::CookieStorage;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::util::perf_monitor::AtomicMetrics;

use super::SequentialDownloader;

fn test_downloader(group: Arc<std::sync::RwLock<RequestGroup>>) -> SequentialDownloader {
    let progress = Arc::new(AtomicProgress::new());
    SequentialDownloader::new(
        crate::http::client_pool::get_global_client(),
        std::env::temp_dir().join("aria2-sequential-retry-test.bin"),
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(CookieStorage::new()), None),
        ProgressUpdater::new(
            None,
            None,
            Arc::clone(&progress),
            Arc::new(AtomicMetrics::new()),
            None,
        ),
        group,
        progress,
        None,
    )
}

#[tokio::test]
async fn retry_wait_is_interruptible_when_removed() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(900),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    group.write().unwrap().mark_removed();
    let downloader = test_downloader(group);

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        downloader.wait_for_retry(Duration::from_secs(5)),
    )
    .await
    .expect("removed retry wait should stop promptly");

    assert!(matches!(
        result,
        Err(crate::error::Aria2Error::DownloadFailed(message))
            if message == "Download cancelled by user"
    ));
}

#[tokio::test]
async fn retry_wait_wakes_when_removed_after_wait_starts() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(901),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    let group_for_wait = Arc::clone(&group);
    let downloader = test_downloader(group);
    let wait_task =
        tokio::spawn(async move { downloader.wait_for_retry(Duration::from_secs(5)).await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    group_for_wait.write().unwrap().mark_removed();

    let result = tokio::time::timeout(Duration::from_millis(100), wait_task)
        .await
        .expect("lifecycle notification should wake retry wait")
        .expect("retry wait task should not panic");
    assert!(matches!(
        result,
        Err(crate::error::Aria2Error::DownloadFailed(message))
            if message == "Download cancelled by user"
    ));
}

#[test]
fn test_merge_ranges_empty() {
    let ranges = &[];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert!(result.is_empty());
}

#[test]
fn test_merge_ranges_single() {
    let ranges = &[(0, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 100)]);
}

#[test]
fn test_merge_ranges_non_overlapping_sorted() {
    let ranges = &[(0, 100), (200, 100), (400, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
}

#[test]
fn test_merge_ranges_non_overlapping_unsorted() {
    let ranges = &[(200, 100), (0, 100), (400, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
}

#[test]
fn test_merge_ranges_overlapping_inner() {
    let ranges = &[(0, 200), (50, 50)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_overlapping_inner_unsorted() {
    let ranges = &[(50, 50), (0, 200)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_overlapping_partial() {
    let ranges = &[(0, 100), (50, 150)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_overlapping_partial_unsorted() {
    let ranges = &[(50, 150), (0, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_adjacent() {
    let ranges = &[(0, 100), (100, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_adjacent_unsorted() {
    let ranges = &[(100, 100), (0, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 200)]);
}

#[test]
fn test_merge_ranges_duplicate() {
    let ranges = &[(0, 100), (0, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 100)]);
}

#[test]
fn test_merge_ranges_multiple_overlapping() {
    let ranges = &[(0, 100), (50, 150), (200, 100), (180, 150)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 330)]);
}

#[test]
fn test_merge_ranges_zero_length() {
    let ranges = &[(0, 0), (100, 0), (200, 100)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 0), (100, 0), (200, 100)]);
}

#[test]
fn test_merge_ranges_complex() {
    let ranges = &[(10, 5), (0, 20), (15, 25), (50, 10), (45, 20), (100, 50)];
    let result = SequentialDownloader::merge_ranges(ranges);
    assert_eq!(result, vec![(0, 40), (45, 20), (100, 50)]);
}

#[test]
fn test_find_all_gaps_empty_ranges() {
    let ranges = &[];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
    assert_eq!(gaps, vec![(0, 1000)]);
}

#[test]
fn test_find_all_gaps_no_gaps() {
    let ranges = &[(0, 1000)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
    assert!(gaps.is_empty());
}

#[test]
fn test_find_all_gaps_single_gap() {
    let ranges = &[(0, 500), (600, 400)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
    assert_eq!(gaps, vec![(500, 100)]);
}

#[test]
fn test_find_all_gaps_multiple_gaps() {
    let ranges = &[(0, 100), (200, 100), (400, 200)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
    assert_eq!(gaps, vec![(100, 100), (300, 100), (600, 400)]);
}

#[test]
fn test_find_all_gaps_overlapping_ranges() {
    let ranges = &[(0, 200), (100, 150), (300, 50)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 500);
    assert_eq!(gaps, vec![(250, 50), (350, 150)]);
}

#[test]
fn test_find_all_gaps_clips_ranges_to_file_length() {
    let ranges = &[(u64::MAX - 1, 8)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 100);
    assert_eq!(gaps, vec![(0, 100)]);
}
