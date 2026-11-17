use std::net::{IpAddr, SocketAddr};

/// Matches hostnames and IP addresses against a `NO_PROXY` value.
///
/// Entries are comma- or newline-separated exact hostnames, domain suffixes
/// (`.example.com`), IP addresses, CIDR networks, or `*` to bypass every proxy.
pub struct NoProxyMatcher {
    entries: Vec<NoProxyEntry>,
    match_all: bool,
}

enum NoProxyEntry {
    Domain(String),
    IpAddr(IpAddr),
    IpNetwork(IpAddr, u8),
}

impl NoProxyMatcher {
    /// Parse a comma- or newline-separated `NO_PROXY` value.
    pub fn from_env_value(value: &str) -> Self {
        let mut entries = Vec::new();
        let mut match_all = false;

        for pattern in value
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if pattern == "*" {
                match_all = true;
                continue;
            }

            if let Some((address, prefix)) = pattern.rsplit_once('/')
                && let (Ok(address), Ok(prefix)) = (address.parse::<IpAddr>(), prefix.parse::<u8>())
                && prefix
                    <= match address {
                        IpAddr::V4(_) => 32,
                        IpAddr::V6(_) => 128,
                    }
            {
                entries.push(NoProxyEntry::IpNetwork(address, prefix));
                continue;
            }

            if let Ok(address) = pattern.parse::<IpAddr>() {
                entries.push(NoProxyEntry::IpAddr(address));
                continue;
            }

            let domain = if pattern.starts_with("*.") {
                &pattern[1..]
            } else {
                pattern
            };
            entries.push(NoProxyEntry::Domain(domain.to_ascii_lowercase()));
        }

        Self { entries, match_all }
    }

    /// Check whether a resolved socket address matches an IP or CIDR entry.
    pub fn should_bypass(&self, target: &SocketAddr) -> bool {
        self.match_all
            || self.entries.iter().any(|entry| match entry {
                NoProxyEntry::IpAddr(address) => *address == target.ip(),
                NoProxyEntry::IpNetwork(network, prefix) => {
                    ip_in_network(target.ip(), *network, *prefix)
                }
                NoProxyEntry::Domain(_) => false,
            })
    }

    /// Check a host string against hostname, IP, and CIDR entries.
    pub fn should_bypass_hostname(&self, hostname: &str) -> bool {
        if self.match_all {
            return true;
        }
        if let Ok(address) = hostname.parse::<IpAddr>() {
            return self.should_bypass(&SocketAddr::new(address, 0));
        }

        let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
        self.entries.iter().any(|entry| match entry {
            NoProxyEntry::Domain(pattern) => {
                if let Some(suffix) = pattern.strip_prefix('.') {
                    hostname == suffix || hostname.ends_with(pattern)
                } else {
                    hostname == *pattern
                }
            }
            NoProxyEntry::IpAddr(_) | NoProxyEntry::IpNetwork(_, _) => false,
        })
    }
}

fn ip_in_network(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(network)) => {
            let ip = u32::from(ip);
            let network = u32::from(network);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            ip & mask == network & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(network)) => {
            let ip = u128::from(ip);
            let network = u128::from(network);
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            ip & mask == network & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_domains_case_insensitively_and_on_label_boundaries() {
        let matcher = NoProxyMatcher::from_env_value("example.com,.internal,*.local");
        assert!(matcher.should_bypass_hostname("EXAMPLE.COM"));
        assert!(matcher.should_bypass_hostname("internal"));
        assert!(matcher.should_bypass_hostname("api.internal"));
        assert!(matcher.should_bypass_hostname("host.local"));
        assert!(!matcher.should_bypass_hostname("notexample.com"));
        assert!(!matcher.should_bypass_hostname("external.test"));
    }

    #[test]
    fn parses_newline_separated_option_values() {
        let matcher = NoProxyMatcher::from_env_value("first.example\nsecond.example");

        assert!(matcher.should_bypass_hostname("first.example"));
        assert!(matcher.should_bypass_hostname("second.example"));
    }

    #[test]
    fn matches_ip_addresses_and_valid_cidrs() {
        let matcher = NoProxyMatcher::from_env_value("192.168.1.1,10.0.0.0/8,2001:db8::/32");
        assert!(matcher.should_bypass_hostname("192.168.1.1"));
        assert!(matcher.should_bypass_hostname("10.2.3.4"));
        assert!(matcher.should_bypass_hostname("2001:db8::1"));
        assert!(!matcher.should_bypass_hostname("11.2.3.4"));
    }

    #[test]
    fn ignores_cidr_prefixes_outside_address_family() {
        let matcher = NoProxyMatcher::from_env_value("192.0.2.0/33,2001:db8::/129");
        assert!(!matcher.should_bypass_hostname("192.0.2.1"));
        assert!(!matcher.should_bypass_hostname("2001:db8::1"));
    }

    #[test]
    fn matches_resolved_socket_addresses() {
        let matcher = NoProxyMatcher::from_env_value("10.0.0.0/8");
        assert!(matcher.should_bypass(&"10.1.2.3:80".parse().unwrap()));
        assert!(!matcher.should_bypass(&"192.0.2.1:80".parse().unwrap()));
    }

    #[test]
    fn wildcard_bypasses_all_hosts_and_addresses() {
        let matcher = NoProxyMatcher::from_env_value("*");
        assert!(matcher.should_bypass_hostname("anything.test"));
        assert!(matcher.should_bypass(&"192.0.2.1:80".parse().unwrap()));
    }
}
