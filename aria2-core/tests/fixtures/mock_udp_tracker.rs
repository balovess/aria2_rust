use std::net::{SocketAddr, UdpSocket};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

#[allow(dead_code)]
pub struct MockUdpTracker {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

#[allow(dead_code)]
impl MockUdpTracker {
    pub async fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind UDP tracker");
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .expect("set UDP tracker timeout");
        let addr = socket.local_addr().expect("read UDP tracker address");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let task = thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while !thread_stop.load(Ordering::Relaxed) {
                let (len, peer) = match socket.recv_from(&mut buffer) {
                    Ok(packet) => packet,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                };
                if len < 16 {
                    continue;
                }
                let action = i32::from_be_bytes(buffer[8..12].try_into().unwrap());
                let transaction = &buffer[12..16];
                let mut response = Vec::new();
                match action {
                    0 => {
                        response.extend_from_slice(&0i32.to_be_bytes());
                        response.extend_from_slice(transaction);
                        response.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes());
                    }
                    1 if len >= 98 => {
                        response.extend_from_slice(&1i32.to_be_bytes());
                        response.extend_from_slice(transaction);
                        response.extend_from_slice(&1800i32.to_be_bytes());
                        response.extend_from_slice(&5i32.to_be_bytes());
                        response.extend_from_slice(&10i32.to_be_bytes());
                        response.extend_from_slice(&[192, 168, 1, 1, 0x1A, 0xE1]);
                        response.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0xE2]);
                    }
                    2 if len >= 36 => {
                        response.extend_from_slice(&2i32.to_be_bytes());
                        response.extend_from_slice(transaction);
                        for _ in 0..3 {
                            response.extend_from_slice(&10i32.to_be_bytes());
                            response.extend_from_slice(&5i32.to_be_bytes());
                            response.extend_from_slice(&100i32.to_be_bytes());
                        }
                    }
                    _ => continue,
                }
                let _ = socket.send_to(&response, peer);
            }
        });
        Self {
            addr,
            stop,
            task: Some(task),
        }
    }

    pub fn url(&self) -> String {
        format!("udp://{}", self.addr)
    }
}

impl Drop for MockUdpTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
