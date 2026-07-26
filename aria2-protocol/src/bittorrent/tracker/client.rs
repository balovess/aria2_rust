use tracing::{debug, warn};

use super::response::{TrackerEvent, TrackerResponse};
use super::udp_tracker_protocol::{
    AnnounceParams, AnnounceResponse, AsyncUdpTrackerClient, UdpEvent,
};

#[derive(Debug, Clone)]
pub struct TrackerAnnounceParams {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Option<TrackerEvent>,
    pub compact: bool,
    pub numwant: Option<u32>,
    pub key: Option<String>,
}

impl TrackerAnnounceParams {
    pub fn new(info_hash: &[u8; 20], peer_id: &[u8; 20], port: u16) -> Self {
        Self {
            info_hash: *info_hash,
            peer_id: *peer_id,
            port,
            uploaded: 0,
            downloaded: 0,
            left: u64::MAX,
            event: Some(TrackerEvent::Started),
            compact: true,
            numwant: None,
            key: None,
        }
    }

    pub fn to_query_string(&self) -> String {
        let mut params = Vec::new();

        params.push(format!(
            "info_hash={}",
            Self::url_encode_infohash(&self.info_hash)
        ));
        params.push(format!("peer_id={}", hex_encode(&self.peer_id)));
        params.push(format!("port={}", self.port));
        params.push(format!("uploaded={}", self.uploaded));
        params.push(format!("downloaded={}", self.downloaded));
        params.push(format!("left={}", self.left));

        if let Some(ref event) = self.event {
            params.push(format!("event={}", event.as_str()));
        }

        if self.compact {
            params.push("compact=1".to_string());
        } else {
            params.push("compact=0".to_string());
        }

        if let Some(nw) = self.numwant {
            params.push(format!("numwant={}", nw));
        }

        if let Some(ref key) = self.key {
            params.push(format!("key={}", url_encode(key)));
        }

        params.join("&")
    }

    fn url_encode_infohash(hash: &[u8; 20]) -> String {
        hash.iter().map(|b| format!("%{:02X}", b)).collect()
    }
}

#[derive(Debug, Clone)]
pub struct TrackerClient {
    announce_urls: Vec<String>,
}

impl TrackerClient {
    pub fn new(announce_url: &str) -> Self {
        Self {
            announce_urls: vec![announce_url.to_string()],
        }
    }

    pub fn with_announce_list(urls: Vec<Vec<String>>) -> Self {
        let flat: Vec<String> = urls.into_iter().flatten().collect();
        Self {
            announce_urls: flat,
        }
    }

    pub async fn announce(
        &self,
        params: &TrackerAnnounceParams,
    ) -> Result<TrackerResponse, String> {
        for (i, url) in self.announce_urls.iter().enumerate() {
            debug!("Trying tracker #{}: {}", i + 1, url);
            match self.announce_single(url, params).await {
                Ok(resp) => return Ok(resp),
                Err(e) => warn!("Tracker #{} failed: {}", i + 1, e),
            }
        }
        Err("All trackers failed".to_string())
    }

    async fn announce_single(
        &self,
        tracker_url: &str,
        params: &TrackerAnnounceParams,
    ) -> Result<TrackerResponse, String> {
        // Check if this is a UDP tracker
        if tracker_url.starts_with("udp://") {
            return self.announce_udp(tracker_url, params).await;
        }

        // HTTP/HTTPS tracker
        use crate::http::client::HttpClient;
        use crate::http::request::HttpRequest;

        let query = params.to_query_string();
        let full_url = if tracker_url.contains('?') {
            format!("{}&{}", tracker_url, query)
        } else {
            format!("{}?{}", tracker_url, query)
        };

        let client = HttpClient::default_client()?;
        let request = HttpRequest::get(&full_url).with_header("User-Agent", "aria2/1.37.0-Rust");

        let response = client.execute(request).await?;

        if !response.is_success() {
            return Err(format!(
                "Tracker returned error status code: {}",
                response.status_code
            ));
        }

        TrackerResponse::parse(&response.body)
            .map_err(|e| format!("Failed to parse tracker response: {}", e))
    }

    /// Announce to a UDP tracker
    async fn announce_udp(
        &self,
        tracker_url: &str,
        params: &TrackerAnnounceParams,
    ) -> Result<TrackerResponse, String> {
        debug!("Using UDP tracker: {}", tracker_url);

        let udp_client = AsyncUdpTrackerClient::new(tracker_url)?;

        // Convert TrackerEvent to UdpEvent
        let udp_event = match params.event {
            Some(TrackerEvent::Started) => UdpEvent::Started,
            Some(TrackerEvent::Completed) => UdpEvent::Completed,
            Some(TrackerEvent::Stopped) => UdpEvent::Stopped,
            None => UdpEvent::None,
        };

        let num_want = params.numwant.map(|n| n as i32).unwrap_or(-1);

        let udp_response = udp_client
            .announce(&AnnounceParams {
                info_hash: &params.info_hash,
                peer_id: &params.peer_id,
                port: params.port,
                uploaded: params.uploaded,
                downloaded: params.downloaded,
                left: params.left,
                event: udp_event,
                num_want,
            })
            .await?;

        // Convert UDP response to TrackerResponse
        Ok(self.convert_udp_response(udp_response))
    }

    /// Convert UDP announce response to HTTP tracker response format
    fn convert_udp_response(&self, udp_resp: AnnounceResponse) -> TrackerResponse {
        use super::response::PeerInfo;

        let peers = udp_resp
            .peers
            .into_iter()
            .map(|(ip, port)| PeerInfo {
                ip,
                port,
                peer_id: None,
            })
            .collect();

        TrackerResponse {
            interval: udp_resp.interval,
            min_interval: None,
            seeders: udp_resp.seeders,
            leechers: udp_resp.leechers,
            peers,
            tracker_id: None,
            warning_message: None,
            failure_reason: None,
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_announce_params_query_string() {
        let ih = [1u8; 20];
        let pid = [2u8; 20];
        let params = TrackerAnnounceParams::new(&ih, &pid, 6881);
        let qs = params.to_query_string();
        assert!(qs.contains("info_hash="));
        assert!(qs.contains("port=6881"));
        assert!(qs.contains("event=started"));
        assert!(qs.contains("compact=1"));
    }

    #[test]
    fn test_tracker_client_creation() {
        let client = TrackerClient::new("http://tracker.example.com/announce");
        assert_eq!(client.announce_urls.len(), 1);

        let client = TrackerClient::with_announce_list(vec![
            vec!["http://a".to_string(), "http://b".to_string()],
            vec!["http://c".to_string()],
        ]);
        assert_eq!(client.announce_urls.len(), 3);
    }
}
