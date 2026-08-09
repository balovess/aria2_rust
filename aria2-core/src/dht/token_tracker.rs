//! Token generation and validation for DHT get_peers/announce_peer.
//!
//! The DHT protocol requires tokens in `announce_peer` requests to prevent
//! spoofing. Tokens are generated as SHA-1 hashes of (info_hash + compact
//! addr + secret). Two secrets are maintained: the current secret and the
//! previous secret. The previous secret allows tokens generated shortly
//! before a secret rotation to still validate.
//!
//! # Secret Rotation
//!
//! Every `DHT_TOKEN_UPDATE_INTERVAL` (10 minutes), the current secret
//! replaces the previous secret, and a new random current secret is
//! generated. This limits the validity window of tokens.
//!
//! C++ reference: `DHTTokenTracker.h/cc`

use sha1::{Digest, Sha1};

use super::constants::{COMPACT_LEN_IPV6, ID_LENGTH, TOKEN_SECRET_COUNT, TOKEN_SECRET_SIZE};

/// Compact address packing: converts IP address string + port into 6 bytes
/// (IPv4) or 18 bytes (IPv6). Returns empty vec on parse failure.
///
/// C++: `bittorrent::packcompact()`
fn pack_compact(ip: &str, port: u16) -> Vec<u8> {
    use std::net::IpAddr;
    let addr: IpAddr = match ip.parse() {
        Ok(a) => a,
        Err(_) => return Vec::new(),
    };
    match addr {
        IpAddr::V4(v4) => {
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(&v4.octets());
            buf.extend_from_slice(&port.to_be_bytes());
            buf
        }
        IpAddr::V6(v6) => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&v6.octets());
            buf.extend_from_slice(&port.to_be_bytes());
            buf
        }
    }
}

/// Token tracker for DHT security (get_peers/announce_peer token validation).
///
/// Maintains two rotating secrets used to generate and validate tokens.
/// Tokens are SHA-1(info_hash || compact_addr || secret), truncated to
/// the SHA-1 output length (20 bytes).
///
/// C++: `DHTTokenTracker`
pub struct TokenTracker {
    /// Two rotating secrets: [0] = current, [1] = previous.
    secrets: [[u8; TOKEN_SECRET_SIZE]; TOKEN_SECRET_COUNT],
}

impl TokenTracker {
    /// Create a new token tracker with random initial secrets.
    ///
    /// C++: `DHTTokenTracker()` — generates random secret[0] and copies
    /// it to secret[1].
    pub fn new() -> Self {
        use rand::RngCore;
        let mut secrets = [[0u8; TOKEN_SECRET_SIZE]; TOKEN_SECRET_COUNT];
        rand::thread_rng().fill_bytes(&mut secrets[0]);
        secrets[1] = secrets[0];
        TokenTracker { secrets }
    }

    /// Create a token tracker with a specific initial secret.
    ///
    /// C++: `DHTTokenTracker(const unsigned char* initialSecret)`
    pub fn with_secret(initial_secret: &[u8; TOKEN_SECRET_SIZE]) -> Self {
        let mut secrets = [[0u8; TOKEN_SECRET_SIZE]; TOKEN_SECRET_COUNT];
        secrets[0] = *initial_secret;
        secrets[1] = *initial_secret;
        TokenTracker { secrets }
    }

    /// Generate a token for the given info_hash, IP, and port.
    ///
    /// Uses the current (most recent) secret to compute:
    /// `SHA1(info_hash || compact_addr || secret)`
    ///
    /// C++: `generateToken(infoHash, ipaddr, port)` — uses secret_[0]
    pub fn generate_token(&self, info_hash: &[u8; ID_LENGTH], ip: &str, port: u16) -> Vec<u8> {
        Self::generate_token_with_secret(info_hash, ip, port, &self.secrets[0])
    }

    /// Validate a token against both current and previous secrets.
    ///
    /// Returns `true` if the token was generated with either the current
    /// or the previous secret.
    ///
    /// C++: `validateToken()` — checks against both secrets
    pub fn validate_token(
        &self,
        token: &[u8],
        info_hash: &[u8; ID_LENGTH],
        ip: &str,
        port: u16,
    ) -> bool {
        for secret in &self.secrets {
            let expected = Self::generate_token_with_secret(info_hash, ip, port, secret);
            if token == expected.as_slice() {
                return true;
            }
        }
        false
    }

    /// Rotate the token secrets.
    ///
    /// The current secret becomes the previous secret, and a new random
    /// secret is generated. This should be called every
    /// `DHT_TOKEN_UPDATE_INTERVAL` (10 minutes).
    ///
    /// C++: `updateTokenSecret()` — secret_[1] = secret_[0], then
    /// generates new random secret_[0].
    pub fn update_secret(&mut self) {
        use rand::RngCore;
        self.secrets[1] = self.secrets[0];
        rand::thread_rng().fill_bytes(&mut self.secrets[0]);
    }

    /// Generate a token using a specific secret.
    ///
    /// C++: `generateToken(infoHash, ipaddr, port, secret)` — the
    /// private implementation.
    ///
    /// Format: `src = info_hash(20) + compact_addr(18 padded) + secret(4)`
    /// Token = `SHA1(src)`
    fn generate_token_with_secret(
        info_hash: &[u8; ID_LENGTH],
        ip: &str,
        port: u16,
        secret: &[u8; TOKEN_SECRET_SIZE],
    ) -> Vec<u8> {
        let compact = pack_compact(ip, port);
        if compact.is_empty() {
            tracing::warn!(
                ip = ip,
                port = port,
                "Token generation failed: invalid address"
            );
            return Vec::new();
        }

        // Build the source buffer matching C++ layout:
        // src[DHT_ID_LENGTH + COMPACT_LEN_IPV6 + SECRET_SIZE]
        // C++ packs the compact address at offset DHT_ID_LENGTH and
        // the secret at offset DHT_ID_LENGTH + COMPACT_LEN_IPV6,
        // with zeroed padding for IPv4 addresses.
        let mut src = vec![0u8; ID_LENGTH + COMPACT_LEN_IPV6 + TOKEN_SECRET_SIZE];
        src[..ID_LENGTH].copy_from_slice(info_hash);
        src[ID_LENGTH..ID_LENGTH + compact.len()].copy_from_slice(&compact);
        src[ID_LENGTH + COMPACT_LEN_IPV6..].copy_from_slice(secret);

        // SHA-1 hash
        let mut hasher = Sha1::new();
        hasher.update(&src);
        let result = hasher.finalize();
        result.to_vec()
    }
}

impl Default for TokenTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TokenTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenTracker")
            .field("secrets", &"[redacted]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_validate_token_ipv4() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);
        assert!(!token.is_empty());
        assert!(tracker.validate_token(&token, &info_hash, "192.168.1.1", 6881));
    }

    #[test]
    fn token_fails_wrong_info_hash() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);

        let wrong_hash = [0xCDu8; ID_LENGTH];
        assert!(!tracker.validate_token(&token, &wrong_hash, "192.168.1.1", 6881));
    }

    #[test]
    fn token_fails_wrong_ip() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);
        assert!(!tracker.validate_token(&token, &info_hash, "192.168.1.2", 6881));
    }

    #[test]
    fn token_fails_wrong_port() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);
        assert!(!tracker.validate_token(&token, &info_hash, "192.168.1.1", 6882));
    }

    #[test]
    fn token_validates_with_previous_secret_after_rotation() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];

        // Generate token before rotation
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);

        // Rotate secrets
        let mut tracker = tracker;
        tracker.update_secret();

        // Token should still validate (using previous secret)
        assert!(tracker.validate_token(&token, &info_hash, "192.168.1.1", 6881));

        // Generate new token — should use current secret
        let new_token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);
        assert!(tracker.validate_token(&new_token, &info_hash, "192.168.1.1", 6881));
    }

    #[test]
    fn token_invalid_after_double_rotation() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];

        // Generate token before any rotation
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);

        // Rotate twice — the original secret is now gone
        let mut tracker = tracker;
        tracker.update_secret();
        tracker.update_secret();

        // Token should no longer validate
        assert!(!tracker.validate_token(&token, &info_hash, "192.168.1.1", 6881));
    }

    #[test]
    fn with_specific_secret() {
        let secret = [0x42u8; TOKEN_SECRET_SIZE];
        let tracker = TokenTracker::with_secret(&secret);
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "10.0.0.1", 1234);
        assert!(tracker.validate_token(&token, &info_hash, "10.0.0.1", 1234));
    }

    #[test]
    fn generate_token_ipv6() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "::1", 6881);
        assert!(!token.is_empty());
        assert!(tracker.validate_token(&token, &info_hash, "::1", 6881));
    }

    #[test]
    fn token_length_is_sha1_output() {
        let tracker = TokenTracker::new();
        let info_hash = [0xABu8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "192.168.1.1", 6881);
        assert_eq!(token.len(), 20); // SHA-1 produces 20 bytes
    }

    #[test]
    fn debug_redacts_secrets() {
        let tracker = TokenTracker::new();
        let debug_str = format!("{:?}", tracker);
        assert!(debug_str.contains("redacted"));
        assert!(!debug_str.contains("0x"));
    }

    #[test]
    fn default_impl_works() {
        let tracker = TokenTracker::default();
        let info_hash = [0u8; ID_LENGTH];
        let token = tracker.generate_token(&info_hash, "127.0.0.1", 6881);
        assert!(tracker.validate_token(&token, &info_hash, "127.0.0.1", 6881));
    }
}
