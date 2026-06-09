//! Integration tests for daemon mode.
//!
//! These tests verify the daemon functionality across platforms.
//! On Windows, we test the detached process creation.
//! On Unix, we test the double-fork daemonization.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::thread;

/// Helper to get the test binary path.
fn get_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("Failed to get current exe path");
    path.pop(); // Remove test executable name
    path.pop(); // Remove 'deps'
    path.push("aria2c");
    
    #[cfg(windows)]
    path.set_extension("exe");
    
    path
}

/// Helper to wait for a file to be created.
fn wait_for_file(path: &PathBuf, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Helper to check if a process is running.
#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output();
    
    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.contains(&pid.to_string())
    } else {
        false
    }
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Test: PID file manager can detect non-existent PID file.
#[test]
fn test_pid_file_manager_no_file() {
    use aria2::PidFileManager;
    
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let pid_file = temp_dir.path().join("nonexistent.pid");
    
    let mgr = PidFileManager::new(pid_file);
    let result = mgr.check_existing();
    
    assert!(result.is_none(), "Should return None for non-existent PID file");
}

/// Test: PID file manager can detect stale PID file.
#[test]
fn test_pid_file_manager_stale_pid() {
    use aria2::PidFileManager;
    
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let pid_file = temp_dir.path().join("stale.pid");
    
    // Write a PID that definitely doesn't exist (9999999)
    fs::write(&pid_file, "9999999").expect("Failed to write PID file");
    
    let mgr = PidFileManager::new(pid_file.clone());
    let result = mgr.check_existing();
    
    assert!(result.is_none(), "Should return None for stale PID");
    assert!(!pid_file.exists(), "Stale PID file should be removed");
}

/// Test: DaemonConfig default values.
#[test]
fn test_daemon_config_defaults() {
    use aria2::DaemonConfig;
    
    let config = DaemonConfig::default();
    assert!(config.pid_file.is_none());
    assert!(config.stdout_file.is_none());
    assert!(config.stderr_file.is_none());
    assert!(!config.chdir_to_root);
    assert!(config.close_fds);
}

/// Test: Daemon mode flag detection from CLI args.
#[test]
fn test_daemon_mode_detection() {
    // Test --daemon
    let args = vec!["--daemon".to_string()];
    assert!(is_daemon_in_args(&args));
    
    // Test -D
    let args = vec!["-D".to_string()];
    assert!(is_daemon_in_args(&args));
    
    // Test --daemon=true
    let args = vec!["--daemon=true".to_string()];
    assert!(is_daemon_in_args(&args));
    
    // Test no daemon flag
    let args = vec!["--help".to_string()];
    assert!(!is_daemon_in_args(&args));
}

fn is_daemon_in_args(args: &[String]) -> bool {
    for arg in args {
        if arg == "--daemon" || arg == "-D" {
            return true;
        }
        if let Some(opt) = arg.strip_prefix("--") {
            if opt == "daemon" || opt.starts_with("daemon=") {
                return true;
            }
        }
    }
    false
}

/// Test: PID file path extraction from CLI args.
#[test]
fn test_pid_file_path_extraction() {
    // Test --pid-file=path
    let args = vec!["--pid-file=/var/run/test.pid".to_string()];
    let path = extract_pid_file(&args);
    assert_eq!(path, Some("/var/run/test.pid"));
    
    // Test --pid-file path
    let args = vec!["--pid-file".to_string(), "/tmp/test.pid".to_string()];
    let path = extract_pid_file(&args);
    assert_eq!(path, Some("/tmp/test.pid"));
    
    // Test no pid-file
    let args = vec!["--daemon".to_string()];
    let path = extract_pid_file(&args);
    assert!(path.is_none());
}

fn extract_pid_file(args: &[String]) -> Option<&str> {
    for i in 0..args.len() {
        if let Some(path) = args[i].strip_prefix("--pid-file=") {
            return Some(path);
        }
        if args[i] == "--pid-file" && i + 1 < args.len() {
            return Some(&args[i + 1]);
        }
    }
    None
}

/// Integration test: Start daemon with RPC and verify PID file is created.
///
/// This test is marked as ignored because it actually starts a background process.
/// Run with: cargo test --test daemon_integration -- --ignored --nocapture
#[test]
#[ignore]
fn test_daemon_start_with_pid_file() {
    let binary = get_binary_path();
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let pid_file = temp_dir.path().join("aria2c.pid");
    let log_file = temp_dir.path().join("aria2c.log");
    
    // Start daemon with RPC enabled
    let status = Command::new(&binary)
        .args([
            "--daemon",
            "--pid-file", pid_file.to_str().unwrap(),
            "--log", log_file.to_str().unwrap(),
            "--enable-rpc",
            "--rpc-listen-port=6999",  // Use non-standard port to avoid conflicts
        ])
        .status()
        .expect("Failed to start daemon");
    
    // Parent process should exit successfully
    assert!(status.success(), "Daemon start should succeed");
    
    // Wait for PID file to be created
    let found = wait_for_file(&pid_file, Duration::from_secs(5));
    assert!(found, "PID file should be created within 5 seconds");
    
    // Read PID from file
    let pid_str = fs::read_to_string(&pid_file).expect("Failed to read PID file");
    let pid: u32 = pid_str.trim().parse().expect("Failed to parse PID");
    
    println!("Daemon started with PID: {}", pid);
    
    // Verify process is running
    assert!(is_process_running(pid), "Daemon process should be running");
    
    // Clean up: stop the daemon
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
    
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg(pid.to_string())
            .output();
    }
    
    // Wait for process to stop
    thread::sleep(Duration::from_secs(1));
}

/// Integration test: Prevent duplicate daemon instances.
///
/// This test verifies that starting a second daemon with the same PID file
/// fails when the first daemon is still running.
#[test]
#[ignore]
fn test_daemon_prevent_duplicate() {
    let binary = get_binary_path();
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let pid_file = temp_dir.path().join("aria2c.pid");
    
    // Start first daemon
    let status = Command::new(&binary)
        .args([
            "--daemon",
            "--pid-file", pid_file.to_str().unwrap(),
            "--enable-rpc",
            "--rpc-listen-port=7000",
        ])
        .status()
        .expect("Failed to start first daemon");
    
    assert!(status.success());
    
    // Wait for PID file
    let found = wait_for_file(&pid_file, Duration::from_secs(5));
    assert!(found, "First daemon should create PID file");
    
    // Try to start second daemon with same PID file
    // This should fail or exit early because daemon is already running
    let status = Command::new(&binary)
        .args([
            "--daemon",
            "--pid-file", pid_file.to_str().unwrap(),
            "--enable-rpc",
            "--rpc-listen-port=7001",  // Different port
        ])
        .status()
        .expect("Failed to execute second daemon check");
    
    // Second instance should exit without error (it detects existing daemon)
    assert!(status.success(), "Second instance should exit cleanly");
    
    // Read PID and clean up
    let pid_str = fs::read_to_string(&pid_file).expect("Failed to read PID file");
    let pid: u32 = pid_str.trim().parse().expect("Failed to parse PID");
    
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
    
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg(pid.to_string())
            .output();
    }
}

/// Integration test: Daemon with custom log file.
#[test]
#[ignore]
fn test_daemon_with_log_file() {
    let binary = get_binary_path();
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let pid_file = temp_dir.path().join("test.pid");
    let log_file = temp_dir.path().join("test.log");
    
    // Start daemon with log file
    let status = Command::new(&binary)
        .args([
            "--daemon",
            "--pid-file", pid_file.to_str().unwrap(),
            "--log", log_file.to_str().unwrap(),
            "--enable-rpc",
            "--rpc-listen-port=7002",
        ])
        .status()
        .expect("Failed to start daemon");
    
    assert!(status.success());
    
    // Wait for log file to be created
    let found = wait_for_file(&log_file, Duration::from_secs(5));
    assert!(found, "Log file should be created");
    
    // Verify log file has content
    thread::sleep(Duration::from_millis(500));
    let log_content = fs::read_to_string(&log_file).unwrap_or_default();
    assert!(!log_content.is_empty(), "Log file should contain output");
    
    // Clean up
    if pid_file.exists() {
        let pid_str = fs::read_to_string(&pid_file).unwrap_or_default();
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/F"])
                    .output();
            }
            
            #[cfg(unix)]
            {
                let _ = Command::new("kill").arg(pid.to_string()).output();
            }
        }
    }
}
