use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tracing::{debug, info, warn};

const EMBEDDED_TRACKER_LIST: &str = "http://1337.abcvg.info:80/announce
http://lucke.fenesisu.moe:6969/announce
http://nyaa.tracker.wf:7777/announce
http://open.acgtracker.com:1096/announce
http://torrentsmd.com:8080/announce
http://tracker.dhitechnical.com:6969/announce
http://tracker.exe.in.th:6969/announce
http://tracker.mywaifu.best:6969/announce
http://tracker.renfei.net:8080/announce
http://tracker.skyts.net:6969/announce
http://tracker.tritan.gg:8080/announce
http://tracker.xn--djrq4gl4hvoi.top:80/announce
http://tracker3.ctix.cn:8080/announce
https://1337.abcvg.info:443/announce
https://cny.fan:443/announce
https://pybittrack.retiolus.net:443/announce
https://shahidrazi.online:443/announce
https://t.213891.xyz:443/announce
https://torrent.tracker.durukanbal.com:443/announce
https://tr.abiir.top:443/announce
https://tr.nyacat.pw:443/announce
https://tracker-zhuqiy.xn--1r3au8b.space:443/announce
https://tracker.7471.top:443/announce
https://tracker.bt4g.com:443/announce
https://tracker.gcrenwp.top:443/announce
https://tracker.ghostchu-services.top:443/announce
https://tracker.kuroy.me:443/announce
https://tracker.manager.v6.navy:443/announce
https://tracker.moeking.me:443/announce
https://tracker.nekomi.cn:443/announce
https://tracker.pmman.tech:443/announce
https://tracker.qingwapt.org:443/announce
https://tracker.yemekyedim.com:443/announce
https://tracker.yggleak.top:443/announce
https://tracker.zhuqiy.com:443/announce
https://tracker1.520.jp:443/announce
udp://admin.52ywp.com:6969/announce
udp://bittorrent-tracker.e-n-c-r-y-p-t.net:1337/announce
udp://bt.rer.lol:6969/announce
udp://evan.im:6969/announce
udp://martin-gebhardt.eu:25/announce
udp://ns575949.ip-51-222-82.net:6969/announce
udp://open.demonii.com:1337/announce
udp://open.stealth.si:80/announce
udp://opentor.org:2710/announce
udp://p4p.arenabg.com:1337/announce
udp://t.overflow.biz:6969/announce
udp://tracker.004430.xyz:1337/announce
udp://tracker.1h.is:1337/announce
udp://tracker.bluefrog.pw:2710/announce
udp://tracker.breizh.pm:6969/announce
udp://tracker.corpscorp.online:80/announce
udp://tracker.darkness.services:6969/announce
udp://tracker.dler.com:6969/announce
udp://tracker.flatuslifir.is:6969/announce
udp://tracker.fnix.net:6969/announce
udp://tracker.gmi.gd:6969/announce
udp://tracker.ixuexi.click:6969/announce
udp://tracker.opentorrent.top:6969/announce
udp://tracker.opentrackr.org:1337/announce
udp://tracker.playground.ru:6969/announce
udp://tracker.plx.im:6969/announce
udp://tracker.qu.ax:6969/announce
udp://tracker.skyts.net:6969/announce
udp://tracker.srv00.com:6969/announce
udp://tracker.t-1.org:6969/announce
udp://tracker.theoks.net:6969/announce
udp://tracker.torrent.eu.org:451/announce
udp://tracker.torrust-demo.com:6969/announce
udp://tracker.tryhackx.org:6969/announce
udp://uabits.today:6990/announce
udp://udp.tracker.projectk.org:23333/announce
udp://wepzone.net:6969/announce
udp://www.nartlof.com:6969/announce
wss://tracker.openwebtorrent.com:443/announce";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerProtocol {
    Http,
    Https,
    Udp,
    Wss,
}

impl TrackerProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerProtocol::Http => "http",
            TrackerProtocol::Https => "https",
            TrackerProtocol::Udp => "udp",
            TrackerProtocol::Wss => "wss",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrackerEntry {
    pub url: String,
    pub protocol: TrackerProtocol,
    pub host: String,
    pub port: u16,
}

pub struct PublicTrackerListStats {
    pub total_entries: usize,
    pub http_count: usize,
    pub udp_count: usize,
    pub is_embedded_fallback: bool,
    pub last_updated: Option<Duration>,
}

pub struct PublicTrackerList {
    entries: tokio::sync::RwLock<Vec<TrackerEntry>>,
    last_updated: tokio::sync::RwLock<Option<Instant>>,
    running: AtomicBool,
}

impl Default for PublicTrackerList {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicTrackerList {
    pub fn new() -> Self {
        let entries = Self::parse(EMBEDDED_TRACKER_LIST);
        Self {
            entries: tokio::sync::RwLock::new(entries),
            last_updated: tokio::sync::RwLock::new(Some(Instant::now())),
            running: AtomicBool::new(true),
        }
    }

    pub fn parse(text: &str) -> Vec<TrackerEntry> {
        let mut result = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.contains("://") {
                continue;
            }

            if let Some(entry) = parse_single_tracker_url(trimmed) {
                result.push(entry);
            }
        }
        result
    }

    pub async fn get_http_trackers(&self) -> Vec<String> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Http || e.protocol == TrackerProtocol::Https)
            .map(|e| e.url.clone())
            .collect()
    }

    pub async fn get_udp_trackers(&self) -> Vec<String> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Udp)
            .map(|e| e.url.clone())
            .collect()
    }

    pub async fn get_all(&self) -> Vec<TrackerEntry> {
        self.entries.read().await.clone()
    }

    pub async fn fetch_and_update(&self, url: &str) -> Result<usize, String> {
        debug!("Fetching public tracker list from {}", url);

        let resp = reqwest::get(url)
            .await
            .map_err(|e| format!("HTTP GET failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP error: {}", resp.status()));
        }

        let text = resp
            .text()
            .await
            .map_err(|e| format!("Read body failed: {}", e))?;

        let new_entries = Self::parse(&text);
        if new_entries.is_empty() {
            return Err("Parsed 0 entries from response".to_string());
        }

        let count = new_entries.len();
        *self.entries.write().await = new_entries;
        *self.last_updated.write().await = Some(Instant::now());

        info!(
            "Public tracker list updated from {}: {} trackers",
            url, count
        );
        Ok(count)
    }

    pub fn start_auto_update(self: &Arc<Self>, url: String, interval: Duration) {
        let e = Arc::clone(self);

        tokio::spawn(async move {
            loop {
                sleep(interval).await;

                if !e.running.load(Ordering::Relaxed) {
                    break;
                }

                match e.fetch_and_update(&url).await {
                    Ok(n) => debug!("Auto-update: refreshed {} public trackers", n),
                    Err(err) => warn!("Auto-update fetch failed (keeping current list): {}", err),
                }
            }
            info!("Public tracker auto-update loop exited");
        });
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub async fn stats(&self) -> PublicTrackerListStats {
        let entries = self.entries.read().await;
        let http_count = entries
            .iter()
            .filter(|e| matches!(e.protocol, TrackerProtocol::Http | TrackerProtocol::Https))
            .count();
        let udp_count = entries
            .iter()
            .filter(|e| e.protocol == TrackerProtocol::Udp)
            .count();

        let embedded = EMBEDDED_TRACKER_LIST
            .lines()
            .filter(|l| l.trim().contains("://"))
            .count();

        PublicTrackerListStats {
            total_entries: entries.len(),
            http_count,
            udp_count,
            is_embedded_fallback: entries.len() == embedded,
            last_updated: self.last_updated.read().await.map(|t| t.elapsed()),
        }
    }
}

fn parse_single_tracker_url(url: &str) -> Option<TrackerEntry> {
    let protocol = if url.starts_with("https://") {
        TrackerProtocol::Https
    } else if url.starts_with("http://") {
        TrackerProtocol::Http
    } else if url.starts_with("udp://") {
        TrackerProtocol::Udp
    } else if url.starts_with("wss://") {
        TrackerProtocol::Wss
    } else {
        return None;
    };

    let after_proto = url.find("://")? + 3;
    let rest = &url[after_proto..];

    let host_end = rest.find('/')?;
    let addr_part = &rest[..host_end];
    let path = &rest[host_end..];

    if path != "/announce" && path != "/announce/" {
        return None;
    }

    let (_host_str, default_port): (&str, u16) = match protocol {
        TrackerProtocol::Http => ("http", 80),
        TrackerProtocol::Https => ("https", 443),
        TrackerProtocol::Udp => ("udp", 6969),
        TrackerProtocol::Wss => ("wss", 443),
    };

    let port = if let Some(colon_pos) = addr_part.rfind(':') {
        let port_str = &addr_part[colon_pos + 1..];
        port_str.parse::<u16>().unwrap_or(default_port)
    } else {
        default_port
    };

    let host = if let Some(colon_pos) = addr_part.rfind(':') {
        &addr_part[..colon_pos]
    } else {
        addr_part
    };

    if host.is_empty() || port == 0 {
        return None;
    }

    Some(TrackerEntry {
        url: url.to_string(),
        protocol,
        host: host.to_string(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_embedded_list() {
        let entries = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST);
        assert!(
            entries.len() >= 50,
            "embedded list should have at least 50 trackers, got {}",
            entries.len()
        );
    }

    #[test]
    fn test_parse_empty_input() {
        let entries = PublicTrackerList::parse("");
        assert!(entries.is_empty(), "empty input should produce empty list");
    }

    #[test]
    fn test_parse_whitespace_only() {
        let entries = PublicTrackerList::parse("\n\n   \n\t\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_mixed_protocols() {
        let text = "http://a.com:80/announce\nhttps://b.com:443/announce\nudp://c.com:6969/announce\nwss://d.com:443/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].protocol, TrackerProtocol::Http);
        assert_eq!(entries[1].protocol, TrackerProtocol::Https);
        assert_eq!(entries[2].protocol, TrackerProtocol::Udp);
        assert_eq!(entries[3].protocol, TrackerProtocol::Wss);
    }

    #[test]
    fn test_parse_invalid_lines_skipped() {
        let text = "http://valid.com:80/announce\nnot a url\n\nhttp://another.com/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_get_http_trackers_filters_correctly() {
        let ptl = PublicTrackerList::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let http = ptl.get_http_trackers().await;
            for url in &http {
                assert!(
                    url.starts_with("http://") || url.starts_with("https://"),
                    "{} should be http or https",
                    url
                );
                assert!(!url.starts_with("udp://"), "{} should not be udp", url);
            }
            assert!(!http.is_empty(), "should have at least some HTTP trackers");
        });
    }

    #[test]
    fn test_get_udp_trackers_filters_correctly() {
        let ptl = PublicTrackerList::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let udp = ptl.get_udp_trackers().await;
            for url in &udp {
                assert!(url.starts_with("udp://"), "{} should be udp", url);
            }
            assert!(!udp.is_empty(), "should have at least some UDP trackers");
        });
    }

    #[tokio::test]
    async fn test_stats_returns_reasonable_values() {
        let ptl = PublicTrackerList::new();
        let stats = ptl.stats().await;
        assert!(stats.total_entries >= 50);
        assert!(stats.http_count > 0);
        assert!(stats.udp_count > 0);
        assert!(
            stats.is_embedded_fallback,
            "default instance uses embedded list"
        );
        assert!(stats.last_updated.is_some());
    }

    #[tokio::test]
    async fn test_default_instance_uses_embedded() {
        let ptl = PublicTrackerList::new();
        let all = ptl.get_all().await;
        let embedded_count = PublicTrackerList::parse(EMBEDDED_TRACKER_LIST).len();
        assert_eq!(
            all.len(),
            embedded_count,
            "default instance should use all embedded entries"
        );
    }

    #[test]
    fn test_parse_non_announce_path_rejected() {
        let text = "http://valid.com:80/scrape\nhttp://valid.com:80/announce";
        let entries = PublicTrackerList::parse(text);
        assert_eq!(entries.len(), 1, "non-announce paths should be rejected");
        assert_eq!(entries[0].url, "http://valid.com:80/announce");
    }

    #[tokio::test]
    async fn test_shutdown_sets_flag() {
        let ptl = PublicTrackerList::new();
        assert!(ptl.running.load(Ordering::Relaxed));
        ptl.shutdown();
        assert!(!ptl.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_start_auto_update_starts_task() {
        use std::time::Duration as StdDuration;
        let ptl = Arc::new(PublicTrackerList::new());
        ptl.start_auto_update(
            "https://example.com/fake.txt".to_string(),
            StdDuration::from_secs(999999),
        );
        sleep(Duration::from_millis(50)).await;
        assert!(ptl.running.load(Ordering::Relaxed));
        ptl.shutdown();
    }
}
