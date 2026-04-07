use aria2_core::filesystem::disk_space::{available_space, has_enough_space, check_with_margin, total_space};
use tempfile::TempDir;

#[test]
fn test_available_space_temp_dir() {
    let dir = TempDir::new().unwrap();
    let avail = available_space(dir.path());
    assert!(avail.is_ok());
    assert!(avail.unwrap() > 0);
}

#[test]
fn test_has_enough_space_true_for_small_request() {
    let dir = TempDir::new().unwrap();
    assert!(has_enough_space(dir.path(), 1));
    assert!(has_enough_space(dir.path(), 1024));
}

#[test]
fn test_check_with_margin_passes() {
    let dir = TempDir::new().unwrap();
    let result = check_with_margin(dir.path(), 1, Some(10));
    assert!(result.is_ok());
}

#[test]
fn test_check_with_margin_rejects_huge_request() {
    let dir = TempDir::new().unwrap();
    let huge_request: u64 = u64::MAX;
    assert!(check_with_margin(dir.path(), huge_request, None).is_err());
}

#[test]
fn test_zero_bytes_always_passes() {
    let dir = TempDir::new().unwrap();
    assert!(check_with_margin(dir.path(), 0, None).is_ok());
}

#[test]
fn test_total_space_positive() {
    let dir = TempDir::new().unwrap();
    let total = total_space(dir.path());
    assert!(total.is_ok());
    assert!(total.unwrap() > 0);
}
