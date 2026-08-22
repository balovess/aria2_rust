//! Process-level CLI/UI coverage against a deterministic loopback HTTP server.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct FixtureServer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    port: u16,
}

fn local_cli(binary: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("ALL_PROXY", "")
        .env("http_proxy", "")
        .env("https_proxy", "")
        .env("all_proxy", "")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    command
}

impl FixtureServer {
    fn start() -> Self {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("fixture server must bind loopback");
        let port = listener
            .local_addr()
            .expect("fixture listener must have an address")
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let body = vec![b'a'; 4096];
            while !thread_stop.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    continue;
                };
                let body = body.clone();
                thread::spawn(move || {
                    let mut request = [0u8; 4096];
                    let count = stream.read(&mut request).unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..count]);
                    let not_found = request.contains("/missing");
                    let (status, response_body): (&str, &[u8]) = if not_found {
                        ("404 Not Found", &[])
                    } else {
                        ("200 OK", &body)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(response_body);
                });
            }
        });
        thread::sleep(Duration::from_millis(50));
        Self {
            stop,
            handle: Some(handle),
            port,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{}", self.port, path)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn cli_success_error_quiet_and_plain_output_are_stable() {
    let server = FixtureServer::start();
    let output_dir = tempfile::tempdir().expect("fixture output directory must be created");
    let binary = env!("CARGO_BIN_EXE_aria2c");

    let success = local_cli(binary)
        .args([
            "--no-conf",
            "--no-color",
            "--summary-interval=0",
            "--show-console-readout=true",
            "--max-tries=3",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--dir",
            output_dir.path().to_str().unwrap(),
            "--out",
            "success.bin",
        ])
        .arg(server.url("success.bin"))
        .output()
        .expect("success CLI process must start");
    assert!(
        success.status.success(),
        "success stderr: {}",
        String::from_utf8_lossy(&success.stderr)
    );
    let success_stdout = String::from_utf8_lossy(&success.stdout);
    assert!(success_stdout.contains("[COMPLETE]"));
    assert!(success_stdout.contains("Download results:"));
    assert!(success_stdout.contains("1 complete, 0 failed"));
    assert!(!success_stdout.contains('\x1b'));
    assert_eq!(
        std::fs::metadata(output_dir.path().join("success.bin"))
            .expect("successful output must exist")
            .len(),
        4096
    );

    let error = local_cli(binary)
        .args([
            "--no-conf",
            "--no-color",
            "--summary-interval=0",
            "--max-tries=3",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--dir",
            output_dir.path().to_str().unwrap(),
            "--out",
            "missing.bin",
        ])
        .arg(server.url("missing.bin"))
        .output()
        .expect("error CLI process must start");
    assert_eq!(error.status.code(), Some(1));
    let error_stdout = String::from_utf8_lossy(&error.stdout);
    assert!(error_stdout.contains("[ERROR]"));
    assert!(error_stdout.contains("0 complete, 1 failed"));
    assert!(String::from_utf8_lossy(&error.stderr).contains("Download failed"));

    let quiet = local_cli(binary)
        .args([
            "--no-conf",
            "--quiet=true",
            "--max-tries=1",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--dir",
            output_dir.path().to_str().unwrap(),
            "--out",
            "quiet.bin",
        ])
        .arg(server.url("quiet.bin"))
        .output()
        .expect("quiet CLI process must start");
    assert!(quiet.status.success());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());

    let multi = local_cli(binary)
        .args([
            "--no-conf",
            "--no-color",
            "--max-concurrent-downloads=1",
            "--summary-interval=0",
            "--max-tries=3",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--dir",
            output_dir.path().to_str().unwrap(),
        ])
        .arg(server.url("multi-one.bin"))
        .arg(server.url("multi-two.bin"))
        .output()
        .expect("multi-task CLI process must start");
    assert!(
        multi.status.success(),
        "multi-task stderr: {}",
        String::from_utf8_lossy(&multi.stderr)
    );
    let multi_stdout = String::from_utf8_lossy(&multi.stdout);
    assert!(multi_stdout.contains("2 complete, 0 failed"));
    assert_eq!(
        std::fs::metadata(output_dir.path().join("multi-one.bin"))
            .expect("first multi-task output must exist")
            .len(),
        4096
    );
    assert_eq!(
        std::fs::metadata(output_dir.path().join("multi-two.bin"))
            .expect("second multi-task output must exist")
            .len(),
        4096
    );

    let stderr_progress = local_cli(binary)
        .args([
            "--no-conf",
            "--no-color",
            "--stderr",
            "--summary-interval=0",
            "--max-tries=3",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--dir",
            output_dir.path().to_str().unwrap(),
            "--out",
            "stderr-progress.bin",
        ])
        .arg(server.url("stderr-progress.bin"))
        .output()
        .expect("stderr progress CLI process must start");
    assert!(
        stderr_progress.status.success(),
        "stderr progress stderr: {}",
        String::from_utf8_lossy(&stderr_progress.stderr)
    );
    assert!(stderr_progress.stdout.is_empty());
    let stderr_output = String::from_utf8_lossy(&stderr_progress.stderr);
    assert!(stderr_output.contains("[COMPLETE]"));
    assert!(stderr_output.contains("Download results:"));

    let log_path = output_dir.path().join("aria2.log");
    let logging = local_cli(binary)
        .args([
            "--no-conf",
            "--no-color",
            "--show-console-readout=false",
            "--log-level=debug",
            "--console-log-level=error",
            "--max-tries=3",
            "--timeout=5",
            "--no-proxy=127.0.0.1",
            "--log",
            log_path.to_str().unwrap(),
            "--dir",
            output_dir.path().to_str().unwrap(),
            "--out",
            "logging.bin",
        ])
        .arg(server.url("logging.bin"))
        .output()
        .expect("logging CLI process must start");
    assert!(
        logging.status.success(),
        "logging stderr: {}",
        String::from_utf8_lossy(&logging.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&logging.stderr).contains(" INFO "),
        "console log level should suppress INFO records"
    );
    let log_contents = std::fs::read_to_string(&log_path).expect("file log must be created");
    assert!(log_contents.contains(" INFO "));
}
