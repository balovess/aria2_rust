#![allow(dead_code)]
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

const SMALL_CONTENT: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const MEDIUM_PATTERN: u8 = 0xAB;
const LARGE_PATTERN: u8 = 0xCD;

#[derive(Default)]
struct FtpSession {
    logged_in: bool,
    passive_listener: Option<TcpListener>,
    passive_stream: Option<TcpStream>,
    active_data_addr: Option<SocketAddr>,
    active_only: bool,
    requires_cwd: bool,
    cwd_ready: bool,
    data_host: Option<String>,
    data_port: Option<u16>,
    pasv_advertised_host: [u8; 4],
    binary_mode: bool,
    rest_offset: u64,
    transfer_delay: Option<Duration>,
    transfer_complete: Option<tokio::sync::oneshot::Receiver<()>>,
}

pub struct MockFtpServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockFtpServer {
    pub async fn start() -> Self {
        Self::start_with_options([127, 0, 0, 1], None, false).await
    }

    /// Start a server that emits transfer data in small delayed chunks.
    ///
    /// The fixture is intentionally slow enough for lifecycle tests to issue
    /// pause/remove while a real FTP data connection is still active.
    pub async fn start_slow() -> Self {
        Self::start_with_options([127, 0, 0, 1], Some(Duration::from_millis(5)), false).await
    }

    /// Start a server whose next transfer chunk is separated by a long delay.
    /// This is used to keep the client inside an in-flight data read while a
    /// lifecycle command is delivered.
    pub async fn start_with_transfer_delay(delay: Duration) -> Self {
        Self::start_with_options([127, 0, 0, 1], Some(delay), false).await
    }

    pub async fn start_with_pasv_advertised_host(advertised_host: [u8; 4]) -> Self {
        Self::start_with_options(advertised_host, None, false).await
    }

    /// Start a server that rejects passive mode and requires EPRT/PORT.
    pub async fn start_active() -> Self {
        Self::start_with_options([127, 0, 0, 1], None, true).await
    }

    /// Start a server that rejects absolute file paths until the client has
    /// followed the PWD/CWD negotiation sequence.
    pub async fn start_requires_cwd() -> Self {
        Self::start_with_options_and_cwd([127, 0, 0, 1], None, false, true).await
    }

    async fn start_with_options(
        advertised_host: [u8; 4],
        transfer_delay: Option<Duration>,
        active_only: bool,
    ) -> Self {
        Self::start_with_options_and_cwd(advertised_host, transfer_delay, active_only, false).await
    }

    async fn start_with_options_and_cwd(
        advertised_host: [u8; 4],
        transfer_delay: Option<Duration>,
        active_only: bool,
        requires_cwd: bool,
    ) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock FTP server port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                let session = tokio::sync::Mutex::new(FtpSession {
                                    pasv_advertised_host: advertised_host,
                                    transfer_delay,
                                    active_only,
                                    requires_cwd,
                                    ..FtpSession::default()
                                });
                                use tokio::io::AsyncWriteExt;
                                stream.write_all(b"220 aria2-rust mock FTP ready\r\n").await.ok();
                                stream.flush().await.ok();
                                Self::handle_client(&mut stream, &session).await;
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        MockFtpServer {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("ftp://127.0.0.1:{}", self.addr.port())
    }

    async fn handle_client(stream: &mut TcpStream, session: &tokio::sync::Mutex<FtpSession>) {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                _ => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (verb, args) = if let Some(space_idx) = trimmed.find(' ') {
                (&trimmed[..space_idx], trimmed[space_idx + 1..].trim())
            } else {
                (trimmed, "")
            };
            let verb_upper: &str = &verb.to_uppercase();

            debug!("[MockFTP] Command: {} {}", verb, args);

            let response = {
                let mut sess = session.lock().await;
                Self::process_command(verb_upper, args, &mut sess).await
            };

            let write_stream = reader.get_mut();
            if let Some(resp) = response {
                write_stream.write_all(resp.as_bytes()).await.ok();
                write_stream.flush().await.ok();
            } else {
                break;
            }

            let transfer_complete = {
                let mut sess = session.lock().await;
                sess.transfer_complete.take()
            };
            if let Some(done) = transfer_complete
                && done.await.is_ok()
            {
                let write_stream = reader.get_mut();
                write_stream
                    .write_all(b"226 Transfer complete\r\n")
                    .await
                    .ok();
                write_stream.flush().await.ok();
            }
        }
    }

    async fn process_command(verb: &str, args: &str, sess: &mut FtpSession) -> Option<String> {
        if verb == "USER" {
            return Some("331 Password required\r\n".into());
        }
        if verb == "PASS" {
            sess.logged_in = true;
            return Some("230 Login successful\r\n".into());
        }
        if !sess.logged_in {
            return Some("530 Not logged in\r\n".into());
        }
        if verb == "TYPE" {
            sess.binary_mode = true;
            return Some("200 Type set to I\r\n".into());
        }
        if verb == "EPRT" {
            let mut fields = args.split('|');
            let _empty = fields.next()?;
            let protocol = fields.next()?;
            let host = fields.next()?;
            let port = fields.next()?.parse::<u16>().ok()?;
            let trailing = fields.next()?;
            if !trailing.is_empty() || fields.next().is_some() || protocol != "1" {
                return Some("501 Invalid EPRT argument\r\n".into());
            }
            let host = host.parse().ok()?;
            sess.active_data_addr = Some(SocketAddr::new(host, port));
            return Some("200 EPRT command successful\r\n".into());
        }
        if verb == "PORT" {
            let parts = args
                .split(',')
                .map(|part| part.parse::<u8>().ok())
                .collect::<Option<Vec<_>>>()?;
            if parts.len() != 6 {
                return Some("501 Invalid PORT argument\r\n".into());
            }
            let host = std::net::Ipv4Addr::new(parts[0], parts[1], parts[2], parts[3]);
            let port = u16::from(parts[4]) * 256 + u16::from(parts[5]);
            sess.active_data_addr = Some(SocketAddr::new(host.into(), port));
            return Some("200 PORT command successful\r\n".into());
        }
        if verb == "PASV" {
            if sess.active_only {
                return Some("502 Passive mode disabled in this fixture\r\n".into());
            }
            let pasv_listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
            let pasv_addr = pasv_listener.local_addr().ok()?;
            let port = pasv_addr.port();
            let p1 = port / 256;
            let p2 = port % 256;
            sess.passive_listener = Some(pasv_listener);
            let [h1, h2, h3, h4] = sess.pasv_advertised_host;
            sess.data_host = Some(format!("{}.{}.{}.{}", h1, h2, h3, h4));
            sess.data_port = Some(port);
            return Some(format!(
                "227 Entering Passive Mode ({},{},{},{},{},{})\r\n",
                h1, h2, h3, h4, p1, p2
            ));
        }
        if verb == "SIZE" {
            if args.contains("notfound")
                || (sess.requires_cwd && (!sess.cwd_ready || args.contains('/')))
            {
                return Some("550 File not found\r\n".into());
            }
            let size = Self::file_size(args);
            return Some(format!("213 {}\r\n", size));
        }
        if verb == "MDTM" {
            if args.contains("notfound")
                || (sess.requires_cwd && (!sess.cwd_ready || args.contains('/')))
            {
                return Some("550 File not found\r\n".into());
            }
            return Some("213 20240115103000\r\n".into());
        }
        if verb == "REST" {
            if let Some(listener) = sess.passive_listener.as_ref() {
                let accepted =
                    { tokio::time::timeout(Duration::from_secs(1), listener.accept()).await };
                match accepted {
                    Ok(Ok((stream, _))) => sess.passive_stream = Some(stream),
                    _ => return Some("425 Passive data connection is not ready\r\n".into()),
                }
            }
            if sess.passive_stream.is_none() && sess.active_data_addr.is_none() {
                return Some("503 Prepare a data connection first\r\n".into());
            }
            if let Ok(offset) = args.parse::<u64>() {
                sess.rest_offset = offset;
                return Some("350 Restart position accepted\r\n".into());
            }
            return Some("501 Invalid REST argument\r\n".into());
        }
        if verb == "RETR" {
            if args.contains("notfound")
                || (sess.requires_cwd && (!sess.cwd_ready || args.contains('/')))
            {
                return Some("550 File not found\r\n".into());
            }

            let passive_listener = sess.passive_listener.take();
            let passive_stream = sess.passive_stream.take();
            sess.data_host.take();
            sess.data_port.take();
            let active_data_addr = sess.active_data_addr.take();
            if passive_stream.is_none() && passive_listener.is_none() && active_data_addr.is_none()
            {
                return None;
            }

            let content = Self::get_file_content(args);
            let rest = sess.rest_offset;
            let _actual_content: Vec<u8> = if rest > 0 && rest < content.len() as u64 {
                content[rest as usize..].to_vec()
            } else {
                content
            };

            let content = Self::get_file_content(args);
            let rest = sess.rest_offset;
            let actual_content: Vec<u8> = if rest > 0 && rest < content.len() as u64 {
                content[rest as usize..].to_vec()
            } else {
                content
            };
            let transfer_delay = sess.transfer_delay;

            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            sess.transfer_complete = Some(done_rx);

            tokio::spawn(async move {
                let data_stream = match (passive_stream, passive_listener, active_data_addr) {
                    (Some(stream), _, None) => Ok(stream),
                    (None, Some(listener), None) => {
                        listener.accept().await.map(|(stream, _)| stream)
                    }
                    (None, None, Some(addr)) => TcpStream::connect(addr).await,
                    _ => return,
                };
                if let Ok(mut data_stream) = data_stream {
                    if let Some(delay) = transfer_delay {
                        for chunk in actual_content.chunks(16 * 1024) {
                            if data_stream.write_all(chunk).await.is_err() {
                                break;
                            }
                            if data_stream.flush().await.is_err() {
                                break;
                            }
                            tokio::time::sleep(delay).await;
                        }
                    } else {
                        data_stream.write_all(&actual_content).await.ok();
                        data_stream.flush().await.ok();
                    }
                    drop(data_stream);
                }
                let _ = done_tx.send(());
            });

            return Some("150 Opening data connection\r\n".into());
        }
        if verb == "CWD" {
            sess.cwd_ready = true;
            return Some("250 Directory changed\r\n".into());
        }
        if verb == "QUIT" {
            return Some("221 Goodbye\r\n".into());
        }
        if verb == "SYST" {
            return Some("215 UNIX Type: L8\r\n".into());
        }
        if verb == "PWD" {
            return Some("257 \"/\" is current directory\r\n".into());
        }
        Some(format!(
            "502 Command not implemented: {} {}\r\n",
            verb, args
        ))
    }

    fn file_size(path: &str) -> u64 {
        match path {
            p if p.contains("small.bin") => SMALL_CONTENT.len() as u64,
            p if p.contains("short.bin") => 4,
            p if p.contains("medium.bin") => 1024 * 1024,
            p if p.contains("large.bin") => 10 * 1024 * 1024,
            _ => 0,
        }
    }

    fn get_file_content(path: &str) -> Vec<u8> {
        match path {
            p if p.contains("small.bin") => SMALL_CONTENT.to_vec(),
            p if p.contains("short.bin") => vec![0x01, 0x02],
            p if p.contains("medium.bin") => vec![MEDIUM_PATTERN; 1024 * 1024],
            p if p.contains("large.bin") => vec![LARGE_PATTERN; 10 * 1024 * 1024],
            _ => vec![],
        }
    }
}

impl Drop for MockFtpServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub fn small_content() -> &'static [u8] {
    SMALL_CONTENT
}
pub fn medium_pattern() -> u8 {
    MEDIUM_PATTERN
}
pub fn large_pattern() -> u8 {
    LARGE_PATTERN
}
