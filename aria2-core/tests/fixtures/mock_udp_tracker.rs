use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Mock UDP Tracker server for testing
pub struct MockUdpTracker {
    socket: Arc<UdpSocket>,
    running: Arc<Mutex<bool>>,
    connection_ids: Arc<Mutex<HashMap<u32, u64>>>,
    port: u16,
}

impl MockUdpTracker {
    /// Create a new mock UDP tracker on a random port
    pub fn new() -> Result<Self, String> {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to bind socket: {}", e))?;

        let port = socket
            .local_addr()
            .map_err(|e| format!("Failed to get local addr: {}", e))?
            .port();

        Ok(Self {
            socket: Arc::new(socket),
            running: Arc::new(Mutex::new(false)),
            connection_ids: Arc::new(Mutex::new(HashMap::new())),
            port,
        })
    }

    /// Get the port the tracker is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the tracker URL
    pub fn url(&self) -> String {
        format!("udp://127.0.0.1:{}", self.port)
    }

    /// Start the mock tracker server
    pub fn start(&self) {
        let mut running = self.running.lock().unwrap();
        if *running {
            return;
        }
        *running = true;
        drop(running);

        let socket = self.socket.clone();
        let running = self.running.clone();
        let connection_ids = self.connection_ids.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 2048];

            loop {
                // Check if still running
                {
                    let r = running.lock().unwrap();
                    if !*r {
                        break;
                    }
                }

                // Set timeout for recv
                socket
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .ok();

                match socket.recv_from(&mut buf) {
                    Ok((len, addr)) => {
                        Self::handle_packet(&socket, &buf[..len], addr, &connection_ids);
                    }
                    Err(_) => {
                        // Timeout or error, continue
                        continue;
                    }
                }
            }
        });
    }

    /// Stop the mock tracker server
    pub fn stop(&self) {
        let mut running = self.running.lock().unwrap();
        *running = false;
    }

    /// Handle incoming UDP packet
    fn handle_packet(
        socket: &UdpSocket,
        data: &[u8],
        addr: std::net::SocketAddr,
        connection_ids: &Arc<Mutex<HashMap<u32, u64>>>,
    ) {
        if data.len() < 8 {
            return;
        }

        // Parse action (skip connection_id for connect request)
        let action = if data.len() >= 16 {
            // Could be connect or announce
            let potential_action = i32::from_be_bytes([data[8], data[9], data[10], data[11]]);
            if potential_action == 0 {
                // Connect request
                0
            } else {
                // Announce or other
                potential_action
            }
        } else {
            return;
        };

        match action {
            0 => Self::handle_connect(socket, data, addr, connection_ids),
            1 => Self::handle_announce(socket, data, addr, connection_ids),
            2 => Self::handle_scrape(socket, data, addr),
            _ => {}
        }
    }

    /// Handle CONNECT request
    fn handle_connect(
        socket: &UdpSocket,
        data: &[u8],
        addr: std::net::SocketAddr,
        connection_ids: &Arc<Mutex<HashMap<u32, u64>>>,
    ) {
        if data.len() < 16 {
            return;
        }

        let transaction_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);

        // Generate a connection ID
        let connection_id = 0x123456789ABCDEF0u64;

        // Store it
        {
            let mut ids = connection_ids.lock().unwrap();
            ids.insert(transaction_id, connection_id);
        }

        // Build response
        let mut response = Vec::new();
        response.extend_from_slice(&0i32.to_be_bytes()); // action = 0 (connect)
        response.extend_from_slice(&transaction_id.to_be_bytes());
        response.extend_from_slice(&connection_id.to_be_bytes());

        socket.send_to(&response, addr).ok();
    }

    /// Handle ANNOUNCE request
    fn handle_announce(
        socket: &UdpSocket,
        data: &[u8],
        addr: std::net::SocketAddr,
        _connection_ids: &Arc<Mutex<HashMap<u32, u64>>>,
    ) {
        if data.len() < 98 {
            return;
        }

        let transaction_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);

        // Build response with mock peers
        let mut response = Vec::new();
        response.extend_from_slice(&1i32.to_be_bytes()); // action = 1 (announce)
        response.extend_from_slice(&transaction_id.to_be_bytes());
        response.extend_from_slice(&1800u32.to_be_bytes()); // interval
        response.extend_from_slice(&5u32.to_be_bytes()); // leechers
        response.extend_from_slice(&10u32.to_be_bytes()); // seeders

        // Add mock peers
        // Peer 1: 192.168.1.1:6881
        response.extend_from_slice(&[192, 168, 1, 1]);
        response.extend_from_slice(&6881u16.to_be_bytes());

        // Peer 2: 10.0.0.1:6882
        response.extend_from_slice(&[10, 0, 0, 1]);
        response.extend_from_slice(&6882u16.to_be_bytes());

        socket.send_to(&response, addr).ok();
    }

    /// Handle SCRAPE request
    fn handle_scrape(socket: &UdpSocket, data: &[u8], addr: std::net::SocketAddr) {
        if data.len() < 16 {
            return;
        }

        let transaction_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);

        // Calculate number of info hashes
        let num_hashes = (data.len() - 16) / 20;

        // Build response
        let mut response = Vec::new();
        response.extend_from_slice(&2i32.to_be_bytes()); // action = 2 (scrape)
        response.extend_from_slice(&transaction_id.to_be_bytes());

        // Add mock data for each hash
        for _ in 0..num_hashes {
            response.extend_from_slice(&10u32.to_be_bytes()); // seeders
            response.extend_from_slice(&5u32.to_be_bytes()); // leechers
            response.extend_from_slice(&100u32.to_be_bytes()); // completed
        }

        socket.send_to(&response, addr).ok();
    }
}

impl Drop for MockUdpTracker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_tracker_creation() {
        let tracker = MockUdpTracker::new().unwrap();
        assert!(tracker.port() > 0);
        assert!(tracker.url().starts_with("udp://"));
    }
}
