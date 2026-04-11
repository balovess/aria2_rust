use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const SECRET_SIZE: usize = 4;
const ROTATION_INTERVAL_SECS: u64 = 900;

pub struct TokenTracker {
    secrets: [[u8; SECRET_SIZE]; 2],
    last_rotation: Instant,
}

impl Default for TokenTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenTracker {
    pub fn new() -> Self {
        let secret = Self::random_secret();
        Self {
            secrets: [secret; 2],
            last_rotation: Instant::now(),
        }
    }

    pub fn with_secret(initial: [u8; SECRET_SIZE]) -> Self {
        Self {
            secrets: [initial; 2],
            last_rotation: Instant::now(),
        }
    }

    pub fn generate_token(&self, info_hash: &[u8; 20], addr: &SocketAddr) -> String {
        self.generate_with_secret(info_hash, addr, &self.secrets[0])
    }

    pub fn validate_token(&self, token: &str, info_hash: &[u8; 20], addr: &SocketAddr) -> bool {
        if token.is_empty() {
            return false;
        }
        for secret in &self.secrets {
            if self.generate_with_secret(info_hash, addr, secret) == token {
                return true;
            }
        }
        false
    }

    pub fn maybe_rotate(&mut self) -> bool {
        if self.last_rotation.elapsed() >= Duration::from_secs(ROTATION_INTERVAL_SECS) {
            self.rotate();
            return true;
        }
        false
    }

    pub fn rotate(&mut self) {
        self.secrets[1] = self.secrets[0];
        self.secrets[0] = Self::random_secret();
        self.last_rotation = Instant::now();
    }

    fn generate_with_secret(
        &self,
        info_hash: &[u8; 20],
        addr: &SocketAddr,
        secret: &[u8; SECRET_SIZE],
    ) -> String {
        let compact = Self::addr_to_compact(addr);
        let mut hasher = Sha1::new();
        hasher.update(info_hash);
        hasher.update(&compact);
        hasher.update(secret);
        let result = hasher.finalize();
        hex::encode(result)
    }

    fn addr_to_compact(addr: &SocketAddr) -> Vec<u8> {
        match addr {
            SocketAddr::V4(v4) => {
                let mut buf = vec![0u8; 6];
                buf[..4].copy_from_slice(&v4.ip().octets());
                buf[4..6].copy_from_slice(&v4.port().to_be_bytes());
                buf
            }
            SocketAddr::V6(v6) => {
                let mut buf = vec![0u8; 18];
                buf[..16].copy_from_slice(&v6.ip().octets());
                buf[16..18].copy_from_slice(&v6.port().to_be_bytes());
                buf
            }
        }
    }

    fn random_secret() -> [u8; SECRET_SIZE] {
        use rand::RngCore;
        let mut s = [0u8; SECRET_SIZE];
        rand::thread_rng().fill_bytes(&mut s);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_validate_roundtrip() {
        let tt = TokenTracker::new();
        let hash = [0xABu8; 20];
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();

        let token = tt.generate_token(&hash, &addr);
        assert!(!token.is_empty(), "token should not be empty");
        assert_eq!(token.len(), 40, "SHA1 hex = 40 chars");

        assert!(
            tt.validate_token(&token, &hash, &addr),
            "same params should validate"
        );
    }

    #[test]
    fn test_reject_wrong_params() {
        let tt = TokenTracker::new();
        let hash = [0xABu8; 20];
        let addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let wrong_addr: SocketAddr = "10.0.0.2:6881".parse().unwrap();
        let wrong_hash = [0xCDu8; 20];

        let token = tt.generate_token(&hash, &addr);

        assert!(
            !tt.validate_token(&token, &wrong_hash, &addr),
            "wrong hash should reject"
        );
        assert!(
            !tt.validate_token(&token, &hash, &wrong_addr),
            "wrong addr should reject"
        );
    }

    #[test]
    fn test_rotation_grace_period() {
        let mut tt = TokenTracker::new();
        let hash = [0x12u8; 20];
        let addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();

        let token_before = tt.generate_token(&hash, &addr);

        tt.rotate();

        assert!(
            tt.validate_token(&token_before, &hash, &addr),
            "old token still valid after rotation (grace period)"
        );

        let token_after = tt.generate_token(&hash, &addr);

        assert_ne!(
            token_before, token_after,
            "tokens before and after rotation should differ"
        );

        assert!(
            tt.validate_token(&token_after, &hash, &addr),
            "new token should also be valid"
        );
    }

    #[test]
    fn test_maybe_rotate_returns_bool() {
        let mut tt = TokenTracker::new();
        assert!(!tt.maybe_rotate(), "should not rotate immediately");
        // Force rotation by setting a past timestamp is not possible with public API,
        // but we can verify the method exists and returns bool without panicking.
        let _ = tt.maybe_rotate();
    }

    #[test]
    fn test_ipv6_addr_generates_different_token() {
        let tt = TokenTracker::with_secret([1, 2, 3, 4]);
        let hash = [0x99u8; 20];

        let v4_addr: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let v6_addr: SocketAddr = "[::1]:6881".parse().unwrap();

        let v4_token = tt.generate_token(&hash, &v4_addr);
        let v6_token = tt.generate_token(&hash, &v6_addr);

        assert_ne!(v4_token, v6_token, "IPv4 and IPv6 tokens must differ");
    }

    #[test]
    fn test_empty_token_rejected() {
        let tt = TokenTracker::new();
        let hash = [0u8; 20];
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        assert!(!tt.validate_token("", &hash, &addr));
        assert!(!tt.validate_token("not_a_valid_hex_sha1", &hash, &addr));
    }
}
