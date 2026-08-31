//! Low-frequency release checks for the interactive CLI.
//!
//! The automatic path is deliberately small and conservative:
//! - it is started at most once per process;
//! - it runs only for interactive, non-quiet, non-daemon sessions;
//! - a local cache gates checks across processes;
//! - failed checks also enter the cooldown period;
//! - the request has a short timeout and never affects a download result.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info};

pub const DEFAULT_INTERVAL_DAYS: u64 = 7;

const RELEASE_URL: &str = "https://github.com/balovess/aria2_rust/releases/latest";
const MAX_INTERVAL_DAYS: u64 = 365;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const LOCK_MAX_AGE: Duration = Duration::from_secs(60);
const CACHE_FILE_NAME: &str = "aria2-rust-update-check.json";

/// Start one best-effort background check.
///
/// The returned task is intentionally detached. A short-lived download may
/// exit before the request completes, which is preferable to adding startup
/// latency to every invocation.
pub fn spawn(interval_days: u64) {
    let interval_days = interval_days.clamp(1, MAX_INTERVAL_DAYS);
    tokio::spawn(async move {
        if let Err(error) = check_periodic(interval_days).await {
            debug!(%error, "background update check skipped");
        }
    });
}

/// Perform an explicit update check, bypassing the periodic cache gate.
pub async fn check_now() -> Result<Option<String>, String> {
    let latest = fetch_latest_version().await?;
    Ok(is_newer_version(&latest, crate::identity::PRODUCT_VERSION).then_some(latest))
}

async fn check_periodic(interval_days: u64) -> Result<(), String> {
    let Some(cache_path) = cache_path() else {
        return Ok(());
    };

    let now = unix_now();
    if !check_is_due(&cache_path, now, interval_days) {
        return Ok(());
    }

    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let Some(_lock) = CacheLock::acquire(&cache_path)? else {
        return Ok(());
    };

    // Another aria2c process may have completed the check between the first
    // cache read and lock acquisition.
    if !check_is_due(&cache_path, now, interval_days) {
        return Ok(());
    }

    let mut cache = read_cache(&cache_path);
    // Record the attempt before touching the network. This makes DNS failures,
    // offline machines, and rate limits obey the same cooldown as successes.
    cache.last_checked_unix_secs = now;
    write_cache(&cache_path, &cache)?;

    match fetch_latest_version().await {
        Ok(latest) => {
            cache.latest_version = Some(latest.clone());
            write_cache(&cache_path, &cache)?;
            if is_newer_version(&latest, crate::identity::PRODUCT_VERSION) {
                info!(
                    current = crate::identity::PRODUCT_VERSION,
                    latest,
                    "a newer aria2-rust release is available; run `aria2c check-update` to inspect it"
                );
            }
        }
        Err(error) => {
            debug!(%error, "update check request failed; cooldown recorded");
        }
    }

    Ok(())
}

async fn fetch_latest_version() -> Result<String, String> {
    aria2_core::http::client_pool::ensure_rustls_provider();
    let response = aria2_core::http::client_pool::get_global_client()
        .get(RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", crate::identity::DEFAULT_USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if response.status().is_redirection() {
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| "latest release redirect has no location".to_string())?;
        return release_version_from_text(location)
            .ok_or_else(|| format!("latest release redirect has no stable tag: {location}"));
    }

    if !response.status().is_success() {
        return Err(format!("release page returned {}", response.status()));
    }

    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("release response is unexpectedly large".to_string());
    }

    let page = String::from_utf8_lossy(&body);
    release_version_from_text(&page)
        .ok_or_else(|| "release page has no stable /releases/tag/vX.Y.Z link".to_string())
}

fn release_version_from_text(text: &str) -> Option<String> {
    let marker = "/releases/tag/";
    let tag = text
        .split_once(marker)?
        .1
        .split([
            '?', '#', '/', '"', '\'', '<', '>', ')', ' ', '\t', '\r', '\n',
        ])
        .next()?;
    normalize_version(tag)
}

fn cache_path() -> Option<PathBuf> {
    home_dir().map(|home| {
        home.join(crate::constants::CONFIG_DIR_NAME)
            .join(CACHE_FILE_NAME)
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(Path::new(&drive).join(path).into_os_string())
        })
        .map(PathBuf::from)
}

#[derive(Debug, Default)]
struct Cache {
    last_checked_unix_secs: u64,
    latest_version: Option<String>,
}

fn read_cache(path: &Path) -> Cache {
    let Ok(bytes) = fs::read(path) else {
        return Cache::default();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Cache::default();
    };
    Cache {
        last_checked_unix_secs: value
            .get("last_checked_unix_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        latest_version: value
            .get("latest_version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }
}

fn write_cache(path: &Path, cache: &Cache) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err("update cache has no parent directory".to_string());
    };
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;

    let value = serde_json::json!({
        "last_checked_unix_secs": cache.last_checked_unix_secs,
        "latest_version": cache.latest_version,
    });
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp_path, value.to_string()).map_err(|error| error.to_string())?;

    // The cache is protected by CacheLock, so replacing it is serialized
    // between aria2c processes. The remove is needed for Windows rename
    // semantics when the destination already exists.
    if let Err(rename_error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(path);
        fs::rename(&temp_path, path).map_err(|_| rename_error.to_string())?;
    }
    Ok(())
}

fn check_is_due(path: &Path, now: u64, interval_days: u64) -> bool {
    let interval_secs = interval_days.clamp(1, MAX_INTERVAL_DAYS) * 24 * 60 * 60;
    let last_checked = read_cache(path).last_checked_unix_secs;
    last_checked == 0 || now.saturating_sub(last_checked) >= interval_secs
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct CacheLock {
    path: PathBuf,
    _file: File,
}

impl CacheLock {
    fn acquire(cache_path: &Path) -> Result<Option<Self>, String> {
        let lock_path = cache_path.with_extension("lock");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => Ok(Some(Self {
                path: lock_path,
                _file: file,
            })),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                    .map(|age| age > LOCK_MAX_AGE)
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&lock_path);
                }
                Ok(None)
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn normalize_version(value: &str) -> Option<String> {
    let value = value.strip_prefix('v').unwrap_or(value);
    parse_version(value).map(|_| value.to_owned())
}

fn parse_version(value: &str) -> Option<Vec<u64>> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let core = value.split_once('-').map_or(value, |(core, _)| core);
    let parts = core
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (!parts.is_empty()).then_some(parts)
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    let Some(latest) = parse_version(latest) else {
        return false;
    };
    let Some(current) = parse_version(current) else {
        return false;
    };
    let length = latest.len().max(current.len());
    (0..length)
        .map(|index| {
            (
                latest.get(index).copied().unwrap_or(0),
                current.get(index).copied().unwrap_or(0),
            )
        })
        .find(|(latest, current)| latest != current)
        .is_some_and(|(latest, current)| latest > current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_gate_skips_recent_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        write_cache(
            &path,
            &Cache {
                last_checked_unix_secs: 1_000_000,
                latest_version: None,
            },
        )
        .unwrap();

        assert!(!check_is_due(&path, 1_000_000 + 6 * 86_400, 7));
        assert!(check_is_due(&path, 1_000_000 + 7 * 86_400, 7));
    }

    #[test]
    fn malformed_cache_is_treated_as_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        fs::write(&path, b"not json").unwrap();
        assert!(check_is_due(&path, 1_000_000, 7));
    }

    #[test]
    fn version_comparison_ignores_v_prefix_and_missing_zeroes() {
        assert!(is_newer_version("v0.4.0", "0.3.4"));
        assert!(!is_newer_version("0.3.4", "0.3.4"));
        assert!(!is_newer_version("0.3.4", "0.3.4.1"));
        assert_eq!(normalize_version("v1.2.3"), Some("1.2.3".to_string()));
        assert!(normalize_version("latest").is_none());
    }

    #[test]
    fn release_page_extracts_v_prefixed_tag() {
        assert_eq!(
            release_version_from_text(
                "[**aria2 0.3.4**](https://github.com/balovess/aria2_rust/releases/tag/v0.3.4)"
            ),
            Some("0.3.4".to_string())
        );
    }
}
