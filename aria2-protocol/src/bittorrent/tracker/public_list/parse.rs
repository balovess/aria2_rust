use super::{TrackerEntry, TrackerProtocol};

pub(super) const EMBEDDED_TRACKER_LIST: &str = "http://1337.abcvg.info:80/announce
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

pub(super) fn parse_tracker_list(text: &str) -> Vec<TrackerEntry> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || !trimmed.contains("://") {
            continue;
        }

        if let Some(entry) =
            normalize_tracker_url(trimmed).and_then(|url| parse_single_tracker_url(&url))
            && seen.insert(entry.url.clone())
        {
            result.push(entry);
        }
    }
    result
}

fn normalize_tracker_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "udp" | "wss") {
        return None;
    }
    if parsed.host_str()?.is_empty() {
        return None;
    }
    if !parsed.path().is_empty() && parsed.path() != "/announce" && parsed.path() != "/announce/" {
        return None;
    }
    let without_fragment = raw.split('#').next().unwrap_or(raw);
    if parsed.path().is_empty() {
        Some(format!("{without_fragment}/announce"))
    } else if parsed.path() == "/announce/" {
        Some(without_fragment.trim_end_matches('/').to_string())
    } else {
        Some(without_fragment.to_string())
    }
}

fn parse_single_tracker_url(url: &str) -> Option<TrackerEntry> {
    let parsed = url::Url::parse(url).ok()?;
    let protocol = match parsed.scheme() {
        "https" => TrackerProtocol::Https,
        "http" => TrackerProtocol::Http,
        "udp" => TrackerProtocol::Udp,
        "wss" => TrackerProtocol::Wss,
        _ => return None,
    };

    let default_port = match protocol {
        TrackerProtocol::Http => 80,
        TrackerProtocol::Https | TrackerProtocol::Wss => 443,
        TrackerProtocol::Udp => 6969,
    };
    let host = parsed.host_str()?.to_string();
    let port = parsed.port().unwrap_or(default_port);
    if host.is_empty() || port == 0 || parsed.path() != "/announce" {
        return None;
    }

    Some(TrackerEntry {
        url: url.to_string(),
        protocol,
        host,
        port,
    })
}
