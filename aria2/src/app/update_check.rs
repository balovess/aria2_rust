//! Low-frequency release checks for the interactive CLI.
//!
//! The automatic path is deliberately small and conservative:
//! - it is started at most once per process;
//! - it runs only for interactive, non-quiet, non-daemon sessions;
//! - a local cache gates checks across processes;
//! - failed checks also enter the cooldown period;
//! - the request has a short timeout and never affects a download result.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tracing::{debug, info};

pub const DEFAULT_INTERVAL_DAYS: u64 = 7;

const RELEASE_URL: &str = "https://github.com/balovess/aria2_rust/releases/latest";
const MAX_INTERVAL_DAYS: u64 = 365;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const LOCK_MAX_AGE: Duration = Duration::from_secs(60);
const CACHE_FILE_NAME: &str = "aria2-rust-update-check.json";
const MAX_ARTIFACT_BYTES: usize = 160 * 1024 * 1024;
const UPDATE_HELPER_ARG: &str = "__aria2_apply_update";

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
                    latest, "a newer aria2-rust release is available"
                );
                if let Err(error) = offer_update(&latest).await {
                    eprintln!("Update action failed: {error}");
                }
            }
        }
        Err(error) => {
            debug!(%error, "update check request failed; cooldown recorded");
        }
    }

    Ok(())
}

/// Offer release-page access or a verified portable-binary update.
pub async fn offer_update(version: &str) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Ok(());
    }

    let version =
        normalize_version(version).ok_or_else(|| format!("invalid release version: {version}"))?;
    let url = format!("https://github.com/balovess/aria2_rust/releases/tag/v{version}");
    let prompt_version = version.clone();

    let (choice, managed) = tokio::task::spawn_blocking(move || -> Result<(u8, bool), String> {
        let executable = std::env::current_exe().ok();
        let managed = managed_install_hint(executable.as_deref());
        eprintln!("A newer aria2-rust release ({prompt_version}) is available.");
        if let Some(hint) = managed {
            eprintln!("This appears to be a package-manager installation. Update with: {hint}");
            eprint!("Open the release page? [Y/n] ");
        } else {
            eprint!(
                "[1] Open release page  [2] Download and update this portable copy  [0] Cancel: "
            );
        }
        io::stderr().flush().map_err(|error| error.to_string())?;

        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        let answer = answer.trim().to_ascii_lowercase();
        if managed.is_some() {
            return Ok((
                u8::from(answer.is_empty() || answer == "y" || answer == "yes"),
                true,
            ));
        }
        Ok((
            match answer.as_str() {
                "1" => 1,
                "2" => 2,
                _ => 0,
            },
            false,
        ))
    })
    .await
    .map_err(|error| error.to_string())??;
    if choice == 1 {
        return open_url(&url);
    }
    if choice == 2 && !managed {
        download_and_stage_update(&version).await?;
    }
    Ok(())
}

fn managed_install_hint(executable: Option<&Path>) -> Option<&'static str> {
    let path = executable?.to_string_lossy().to_ascii_lowercase();
    if path.contains("scoop\\apps\\aria2-rust\\") || path.contains("scoop/apps/aria2-rust/") {
        Some("scoop update aria2-rust")
    } else if path.contains("/cellar/aria2-rust/") || path.contains("/homebrew/") {
        Some("brew upgrade aria2-rust")
    } else {
        None
    }
}

async fn download_and_stage_update(version: &str) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    ensure_portable_update_target(&executable)?;
    let parent = executable
        .parent()
        .ok_or("executable has no parent directory")?;
    let artifact = artifact_name()?;
    let tag = format!("v{version}");
    let base = format!("https://github.com/balovess/aria2_rust/releases/download/{tag}/{artifact}");
    let archive = fetch_bytes(&base).await?;
    let checksum = fetch_bytes(&format!("{base}.sha256")).await?;
    verify_sha256(&archive, &checksum)?;

    let stage_dir = parent.join(format!(".aria2-update-{}-{version}", std::process::id()));
    fs::create_dir(&stage_dir)
        .map_err(|error| format!("cannot create staging directory: {error}"))?;
    let result = (|| {
        let archive_path = stage_dir.join(&artifact);
        fs::write(&archive_path, &archive).map_err(|error| error.to_string())?;
        let extract_dir = stage_dir.join("unpacked");
        fs::create_dir(&extract_dir).map_err(|error| error.to_string())?;
        let binary_name = if cfg!(windows) {
            "aria2c.exe"
        } else {
            "aria2c"
        };
        let status = Command::new("tar")
            .args(["-xf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(&extract_dir)
            .arg(binary_name)
            .status()
            .map_err(|error| format!("cannot extract release archive with tar: {error}"))?;
        if !status.success() {
            return Err("release archive extraction failed".into());
        }
        let extracted = extract_dir.join(binary_name);
        let metadata = fs::symlink_metadata(&extracted).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file() || metadata.len() == 0 {
            return Err("release archive did not contain a regular aria2c binary".into());
        }
        let staged = stage_dir.join(format!(
            "aria2c.new{}",
            if cfg!(windows) { ".exe" } else { "" }
        ));
        fs::copy(&extracted, &staged).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
                .map_err(|error| error.to_string())?;
        }
        Command::new(&executable)
            .arg(UPDATE_HELPER_ARG)
            .arg(&staged)
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot schedule update helper: {error}"))?;
        eprintln!("Update verified and staged. It will be installed after aria2c exits.");
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage_dir);
    }
    result
}

fn ensure_portable_update_target(executable: &Path) -> Result<(), String> {
    if let Some(command) = managed_install_hint(Some(executable)) {
        return Err(format!("package-manager installation; use `{command}`"));
    }
    let metadata = fs::symlink_metadata(executable).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("automatic update only supports a regular portable binary".into());
    }
    let parent = executable
        .parent()
        .ok_or("executable has no parent directory")?;
    let probe = parent.join(format!(".aria2-update-probe-{}", std::process::id()));
    fs::create_dir(&probe).map_err(|error| {
        format!("binary directory is not writable; update it with its installation method: {error}")
    })?;
    fs::remove_dir(&probe).map_err(|error| format!("cannot clean update probe: {error}"))
}

fn verify_sha256(archive: &[u8], checksum: &[u8]) -> Result<(), String> {
    let expected = std::str::from_utf8(checksum)
        .ok()
        .and_then(|text| text.split_whitespace().next())
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("release checksum file is malformed")?;
    let actual = Sha256::digest(archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err("downloaded release archive failed SHA-256 verification".into());
    }
    Ok(())
}

fn artifact_name() -> Result<String, String> {
    let (arch, os, suffix) = match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "windows") => ("x86_64", "windows", "zip"),
        ("x86_64", "linux") => ("x86_64", "linux", "tar.gz"),
        ("aarch64", "linux") => ("aarch64", "linux", "tar.gz"),
        ("aarch64", "macos") => ("aarch64", "macos", "tar.gz"),
        ("x86_64", "macos") => ("x86_64", "macos", "tar.gz"),
        _ => return Err("automatic updates are unavailable for this platform".into()),
    };
    let profile = if cfg!(feature = "full") {
        "full"
    } else if cfg!(feature = "standard") {
        "standard"
    } else {
        "minimal"
    };
    Ok(format!("aria2-{arch}-{os}-{profile}.{suffix}"))
}

async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    aria2_core::http::client_pool::ensure_rustls_provider();
    let response = aria2_core::http::client_pool::get_global_client()
        .get(url)
        .header("User-Agent", crate::identity::DEFAULT_USER_AGENT)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("download failed: {} ({url})", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARTIFACT_BYTES as u64)
    {
        return Err("release asset exceeds the size limit".into());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_ARTIFACT_BYTES {
            return Err("release asset exceeds the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();

    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(url).spawn();

    #[cfg(not(any(windows, unix)))]
    return Err(format!("open this release page in a browser: {url}"));

    result
        .map(|_| ())
        .map_err(|error| format!("could not open browser ({error}); open this page: {url}"))
}

/// Runs the hidden replacement helper when this executable was relaunched for an update.
/// Returns `None` during normal CLI startup.
pub fn run_update_helper_if_requested() -> Option<Result<(), String>> {
    let mut args = std::env::args_os();
    let _exe = args.next()?;
    if args.next()?.to_string_lossy() != UPDATE_HELPER_ARG {
        return None;
    }
    let staged = PathBuf::from(args.next()?);
    let pid = args.next()?.to_string_lossy().parse::<u32>().ok()?;
    Some(apply_staged_binary(&staged, pid))
}

fn apply_staged_binary(staged: &Path, parent_pid: u32) -> Result<(), String> {
    let target = std::env::current_exe().map_err(|error| error.to_string())?;
    let target_dir = target
        .parent()
        .ok_or("executable has no parent directory")?;
    let staged_dir = staged
        .parent()
        .ok_or("staged binary has no parent directory")?;
    if staged_dir.parent() != Some(target_dir)
        || !staged_dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".aria2-update-"))
        || !staged
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("aria2c.new"))
    {
        return Err("refusing to replace executable from an unexpected staging path".into());
    }
    wait_for_parent_exit(parent_pid)?;
    replace_binary(&target, staged)
}

fn replace_binary(target: &Path, staged: &Path) -> Result<(), String> {
    let target_dir = target
        .parent()
        .ok_or("executable has no parent directory")?;
    let backup = target_dir.join(format!(".aria2c-update-backup-{}", std::process::id()));
    if backup.exists() {
        return Err("an update backup already exists; refusing to overwrite it".into());
    }
    fs::rename(&target, &backup)
        .map_err(|error| format!("cannot back up current executable: {error}"))?;
    if let Err(error) = fs::rename(staged, target) {
        let _ = fs::rename(&backup, target);
        return Err(format!("cannot install verified update: {error}"));
    }
    let _ = fs::remove_file(backup);
    if let Some(stage_dir) = staged.parent() {
        let _ = fs::remove_dir_all(stage_dir);
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_parent_exit(pid: u32) -> Result<(), String> {
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(500));
    }
    if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err("could not verify that aria2c has exited".into())
    }
}

#[cfg(windows)]
fn wait_for_parent_exit(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err("could not wait for aria2c to exit".into());
    }
    let result = unsafe { WaitForSingleObject(handle, INFINITE) };
    unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
    if result == 0 {
        Ok(())
    } else {
        Err("waiting for aria2c to exit failed".into())
    }
}

#[cfg(not(any(unix, windows)))]
fn wait_for_parent_exit(_pid: u32) -> Result<(), String> {
    Err("automatic replacement is unsupported on this platform".into())
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
    Some(
        crate::app::paths::system_layout()
            .cache_dir
            .join(CACHE_FILE_NAME),
    )
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
    let mut parts = value.split('.');
    let valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    });
    (valid && parts.next().is_none()).then(|| value.to_owned())
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
        assert!(normalize_version("1.2.3-beta/../../x").is_none());
        assert!(normalize_version("1.2.3.4").is_none());
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

    #[test]
    fn checksum_accepts_matching_sha256_and_rejects_tampering() {
        let checksum =
            b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  aria2.zip\n";
        assert!(verify_sha256(b"abc", checksum).is_ok());
        assert!(verify_sha256(b"abd", checksum).is_err());
        assert!(verify_sha256(b"abc", b"not-a-checksum").is_err());
    }

    #[test]
    fn artifact_name_matches_the_current_supported_target_and_feature_tier() {
        let artifact = artifact_name().unwrap();
        let tier = if cfg!(feature = "full") {
            "full"
        } else if cfg!(feature = "standard") {
            "standard"
        } else {
            "minimal"
        };
        assert!(artifact.contains(tier));
        assert!(artifact.starts_with(&format!(
            "aria2-{}-{}-",
            std::env::consts::ARCH,
            std::env::consts::OS
        )));
    }

    #[test]
    fn package_manager_paths_are_not_treated_as_portable_binaries() {
        assert_eq!(
            managed_install_hint(Some(Path::new(
                "C:/Users/test/scoop/apps/aria2-rust/current/aria2c.exe"
            ))),
            Some("scoop update aria2-rust")
        );
        assert_eq!(
            managed_install_hint(Some(Path::new(
                "/opt/homebrew/Cellar/aria2-rust/1.2/bin/aria2c"
            ))),
            Some("brew upgrade aria2-rust")
        );
        assert_eq!(
            managed_install_hint(Some(Path::new("/opt/aria2/aria2c"))),
            None
        );
    }

    #[test]
    fn portable_update_requires_a_regular_binary_in_a_writable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join(if cfg!(windows) {
            "aria2c.exe"
        } else {
            "aria2c"
        });
        fs::write(&binary, b"binary").unwrap();
        assert!(ensure_portable_update_target(&binary).is_ok());
        assert!(
            !dir.path()
                .join(format!(".aria2-update-probe-{}", std::process::id()))
                .exists()
        );
        assert!(ensure_portable_update_target(dir.path()).is_err());
    }

    #[test]
    fn replacement_installs_staged_binary_and_removes_backup() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("aria2c");
        let staged_dir = dir.path().join(".aria2-update-test");
        fs::create_dir(&staged_dir).unwrap();
        let staged = staged_dir.join("aria2c.new");
        fs::write(&target, b"old binary").unwrap();
        fs::write(&staged, b"new binary").unwrap();

        replace_binary(&target, &staged).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new binary");
        assert!(!staged_dir.exists());
        assert!(
            !dir.path()
                .join(format!(".aria2c-update-backup-{}", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn replacement_failure_restores_original_binary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("aria2c");
        let staged = dir.path().join("missing.new");
        fs::write(&target, b"old binary").unwrap();

        assert!(replace_binary(&target, &staged).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old binary");
    }
}
