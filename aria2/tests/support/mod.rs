//! Shared process and HTTP helpers for public RPC client compatibility tests.

// Each integration test compiles this shared module as a separate crate, so a
// helper used by one client workflow is intentionally unused by another.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A live `aria2c` process with its RPC listener ready for client requests.
pub struct RunningAria2 {
    child: Child,
    port: u16,
}

/// Raw HTTP response returned by an aria2 RPC route.
pub struct HttpResponse {
    pub status: u16,
    pub headers: String,
    pub body: Vec<u8>,
}

impl RunningAria2 {
    /// Start `aria2c` with RPC enabled and wait for its loopback listener.
    pub fn start_rpc(extra_args: &[String]) -> Self {
        let port = reserve_loopback_port();
        let mut args = vec![
            "--no-conf=true".to_owned(),
            "--enable-rpc=true".to_owned(),
            format!("--rpc-listen-port={port}"),
        ];
        args.extend(extra_args.iter().cloned());

        let child = Command::new(env!("CARGO_BIN_EXE_aria2c"))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start aria2c");
        let process = Self { child, port };
        process.wait_for_rpc_server();
        process
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Post a complete request through an aria2 HTTP RPC adapter.
    pub fn post(&self, path: &str, content_type: &str, body: &[u8]) -> HttpResponse {
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let mut stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT)
            .expect("failed to connect to aria2c RPC listener");
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .expect("failed to configure RPC read timeout");

        let request_head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.port,
            body.len()
        );
        stream
            .write_all(request_head.as_bytes())
            .and_then(|()| stream.write_all(body))
            .expect("failed to send RPC request");

        read_http_response(&mut stream)
    }

    /// Send an HTTP request head without a body and return bytes received
    /// before the server closes the connection.
    ///
    /// This exposes aria2's early `Content-Length` rejection path, which is
    /// intentionally observable before any request body is transmitted.
    pub fn post_head_only_until_close(
        &self,
        path: &str,
        content_type: &str,
        content_length: usize,
    ) -> Vec<u8> {
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let mut stream = TcpStream::connect_timeout(&address, REQUEST_TIMEOUT)
            .expect("failed to connect to aria2c RPC listener");
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .expect("failed to configure RPC read timeout");

        let request_head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n",
            self.port,
        );
        stream
            .write_all(request_head.as_bytes())
            .expect("failed to send RPC request head");

        let mut response = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => return response,
                Ok(count) => response.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                    panic!("RPC server did not close oversized request connection")
                }
                Err(error) => panic!("failed to read oversized request response: {error}"),
            }
        }
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("failed to inspect aria2c process")
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "aria2c did not exit within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_rpc_server(&self) {
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + SERVER_START_TIMEOUT;

        loop {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "aria2c did not start its RPC listener on {address}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for RunningAria2 {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn reserve_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("failed to reserve a loopback port")
        .local_addr()
        .expect("reserved listener must have an address")
        .port()
}

fn read_http_response(stream: &mut TcpStream) -> HttpResponse {
    let mut response = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream
            .read(&mut buffer)
            .expect("failed to read RPC response");
        assert_ne!(count, 0, "RPC server closed the response before completion");
        response.extend_from_slice(&buffer[..count]);

        let Some(body_start) = header_end(&response) else {
            continue;
        };
        let headers = std::str::from_utf8(&response[..body_start])
            .expect("HTTP response headers must be UTF-8");
        let body_end = body_start + response_content_length(headers);
        if response.len() < body_end {
            continue;
        }

        let status = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .expect("HTTP response must include a numeric status code");
        return HttpResponse {
            status,
            headers: headers.to_owned(),
            body: response[body_start..body_end].to_vec(),
        };
    }
}

fn header_end(response: &[u8]) -> Option<usize> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn response_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .expect("RPC response must contain Content-Length")
}
