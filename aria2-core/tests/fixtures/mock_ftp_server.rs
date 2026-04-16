#![allow(dead_code)]
use std::net::SocketAddr;
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
    data_host: Option<String>,
    data_port: Option<u16>,
    binary_mode: bool,
    rest_offset: u64,
}

pub struct MockFtpServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockFtpServer {
    pub async fn start() -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr)
            .await
            .expect("绑定Mock FTP服务器端口失败");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                let session = tokio::sync::Mutex::new(FtpSession::default());
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

            debug!("[MockFTP] 命令: {} {}", verb, args);

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
        if verb == "PASV" {
            let pasv_listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
            let pasv_addr = pasv_listener.local_addr().ok()?;
            let port = pasv_addr.port();
            let p1 = port / 256;
            let p2 = port % 256;
            sess.passive_listener = Some(pasv_listener);
            sess.data_host = Some("127.0.0.1".to_string());
            sess.data_port = Some(port);
            return Some(format!(
                "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                p1, p2
            ));
        }
        if verb == "SIZE" {
            let size = Self::file_size(args);
            return Some(format!("213 {}\r\n", size));
        }
        if verb == "REST" {
            if let Ok(offset) = args.parse::<u64>() {
                sess.rest_offset = offset;
                return Some("350 Restart position accepted\r\n".into());
            }
            return Some("501 Invalid REST argument\r\n".into());
        }
        if verb == "RETR" {
            if args.contains("notfound") {
                return Some("550 File not found\r\n".into());
            }

            let (listener, _host, _port) = {
                let l = sess.passive_listener.take()?;
                let h = sess.data_host.take()?;
                let p = sess.data_port.take()?;
                (l, h, p)
            };

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

            tokio::spawn(async move {
                if let Ok((mut data_stream, _addr)) = listener.accept().await {
                    data_stream.write_all(&actual_content).await.ok();
                    data_stream.flush().await.ok();
                    drop(data_stream);
                }
            });

            return Some("150 Opening data connection\r\n".into());
        }
        if verb == "CWD" {
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
            p if p.contains("medium.bin") => 1024 * 1024,
            p if p.contains("large.bin") => 10 * 1024 * 1024,
            _ => 0,
        }
    }

    fn get_file_content(path: &str) -> Vec<u8> {
        match path {
            p if p.contains("small.bin") => SMALL_CONTENT.to_vec(),
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
