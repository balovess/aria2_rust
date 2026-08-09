use std::net::{IpAddr, SocketAddr};

/// Matcher for NO_PROXY / no_proxy environment variable patterns
/// Supports patterns like:
///   - Exact domain matches: "example.com"
///   - Wildcard subdomain matches: ".example.com" (matches *.example.com)
///   - IP address exact matches: "192.168.1.1"
///   - IP/CIDR notation: "192.168.0.0/16"
///   - Special token "*": matches everything (bypass all proxies)
pub struct NoProxyMatcher {
    entries: Vec<NoProxyEntry>,
    match_all: bool,
}

enum NoProxyEntry {
    Domain(String),        // Exact domain or .domain for wildcard subdomains
    IpAddr(IpAddr),        // Exact IP address
    IpNetwork(IpAddr, u8), // IP with prefix length (CIDR)
}

impl NoProxyMatcher {
    /// Create a new NoProxyMatcher from the value of NO_PROXY/no_proxy env var.
    /// The input is typically comma-separated list of patterns.
    pub fn from_env_value(value: &str) -> Self {
        let mut entries = Vec::new();
        let mut match_all = false;

        for pattern in value.split(',') {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }

            if pattern == "*" {
                match_all = true;
                continue;
            }

            // Check for CIDR notation
            if let Some(slash_pos) = pattern.rfind('/') {
                let addr_str = &pattern[..slash_pos];
                let prefix_str = &pattern[slash_pos + 1..];
                if let Ok(addr) = addr_str.parse::<IpAddr>()
                    && let Ok(prefix) = prefix_str.parse::<u8>()
                {
                    entries.push(NoProxyEntry::IpNetwork(addr, prefix));
                    continue;
                }
            }

            // Try parsing as IP address
            if let Ok(addr) = pattern.parse::<IpAddr>() {
                entries.push(NoProxyEntry::IpAddr(addr));
                continue;
            }

            // Treat as domain pattern (normalize *.prefix to .prefix for wildcard matching)
            let normalized = if pattern.starts_with("*.") {
                &pattern[1..]
            } else {
                pattern
            };
            entries.push(NoProxyEntry::Domain(normalized.to_lowercase()));
        }

        Self { entries, match_all }
    }

    /// Check if a given target address should bypass the proxy (i.e., matches NO_PROXY).
    pub fn should_bypass(&self, target: &SocketAddr) -> bool {
        if self.match_all {
            return true;
        }

        let ip = target.ip();

        for entry in &self.entries {
            match entry {
                NoProxyEntry::IpAddr(entry_ip) if *entry_ip == ip => return true,
                NoProxyEntry::IpNetwork(network_addr, prefix_len)
                    if Self::ip_in_network(ip, *network_addr, *prefix_len) =>
                {
                    return true;
                }
                _ => {}
            }
        }

        false
    }

    /// Check if a hostname should bypass the proxy.
    pub fn should_bypass_hostname(&self, hostname: &str) -> bool {
        if self.match_all {
            return true;
        }

        let hostname_lower = hostname.to_lowercase();

        for entry in &self.entries {
            if let NoProxyEntry::Domain(pattern) = entry {
                if let Some(stripped) = pattern.strip_prefix('.') {
                    // Wildcard subdomain match: .example.com matches *.example.com
                    if hostname_lower.ends_with(pattern.as_str()) || hostname_lower == stripped {
                        return true;
                    }
                } else {
                    // Exact domain match
                    if hostname_lower == *pattern {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Check if an IP falls within a CIDR network range.
    fn ip_in_network(ip: IpAddr, network: IpAddr, prefix_len: u8) -> bool {
        match (ip, network) {
            (std::net::IpAddr::V4(ip_v4), std::net::IpAddr::V4(net_v4)) => {
                let ip_u32 = u32::from_be_bytes(ip_v4.octets());
                let net_u32 = u32::from_be_bytes(net_v4.octets());
                let mask = if prefix_len >= 32 {
                    0xFFFFFFFFu32
                } else {
                    !(0xFFFFFFFF >> prefix_len)
                };
                (ip_u32 & mask) == (net_u32 & mask)
            }
            (std::net::IpAddr::V6(ip_v6), std::net::IpAddr::V6(net_v6)) => {
                let ip_octets = ip_v6.octets();
                let net_octets = net_v6.octets();
                let full_bytes = (prefix_len as usize) / 8;
                let remaining_bits = (prefix_len as usize) % 8;

                // Compare full bytes
                if ip_octets[..full_bytes] != net_octets[..full_bytes] {
                    return false;
                }

                // Compare remaining bits
                if remaining_bits > 0 && full_bytes < 16 {
                    let mask = !(0xFFu8 >> remaining_bits);
                    if (ip_octets[full_bytes] & mask) != (net_octets[full_bytes] & mask) {
                        return false;
                    }
                }

                true
            }
            _ => false,
        }
    }
}
