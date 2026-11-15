use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const SECRET_SIZE: usize = 32;
const ROTATION_INTERVAL_SECS: u64 = 900;

#[derive(Clone, Copy)]
struct Secret {
    bytes: [u8; SECRET_SIZE],
    len: usize,
}

pub struct TokenTracker {
    secrets: [Secret; 2],
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

    /// Creates a tracker with a deterministic four-byte secret.
    ///
    /// This constructor is intended for tests and deterministic callers. Use
    /// [`TokenTracker::new`] for production so the secret has full entropy.
    pub fn with_secret(initial: [u8; 4]) -> Self {
        let mut bytes = [0; SECRET_SIZE];
        bytes[..initial.len()].copy_from_slice(&initial);
        let secret = Secret {
            bytes,
            len: initial.len(),
        };
        Self {
            secrets: [secret; 2],
            last_rotation: Instant::now(),
        }
    }

    /// Creates a tracker with a caller-provided 256-bit secret.
    pub fn with_secret_bytes(initial: [u8; SECRET_SIZE]) -> Self {
        let secret = Secret {
            bytes: initial,
            len: SECRET_SIZE,
        };
        Self {
            secrets: [secret; 2],
            last_rotation: Instant::now(),
        }
    }

    pub fn generate_token(&self, info_hash: &[u8; 20], addr: &SocketAddr) -> String {
        self.generate_with_secret(info_hash, addr, &self.secrets[0])
    }

    pub fn validate_token(&self, token: &str, info_hash: &[u8; 20], addr: &SocketAddr) -> bool {
        self.validate_token_bytes(token.as_bytes(), info_hash, addr)
    }

    /// Validate the opaque token bytes carried by BEP 44 and BEP 5 messages.
    /// Tokens are not required to be UTF-8 by the wire protocol.
    pub fn validate_token_bytes(
        &self,
        token: &[u8],
        info_hash: &[u8; 20],
        addr: &SocketAddr,
    ) -> bool {
        if token.is_empty() {
            return false;
        }
        for secret in &self.secrets {
            if self
                .generate_with_secret(info_hash, addr, secret)
                .as_bytes()
                == token
            {
                return true;
            }
        }
        false
    }

    pub fn maybe_rotate(&mut self) -> bool {
        self.maybe_rotate_after(Duration::from_secs(ROTATION_INTERVAL_SECS))
    }

    /// Rotate the token secret when the caller's configured interval has elapsed.
    pub fn maybe_rotate_after(&mut self, interval: Duration) -> bool {
        if self.last_rotation.elapsed() >= interval {
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
        secret: &Secret,
    ) -> String {
        let mut hasher = Sha1::new();
        hasher.update(info_hash);
        match addr {
            SocketAddr::V4(v4) => hasher.update(v4.ip().octets()),
            SocketAddr::V6(v6) => hasher.update(v6.ip().octets()),
        }
        hasher.update(&secret.bytes[..secret.len]);
        let result = hasher.finalize();
        hex::encode(result)
    }

    fn random_secret() -> Secret {
        use rand::Rng;
        let mut bytes = [0u8; SECRET_SIZE];
        rand::rng().fill_bytes(&mut bytes);
        Secret {
            bytes,
            len: SECRET_SIZE,
        }
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
    fn token_is_bound_to_ip_not_transient_udp_port() {
        let tt = TokenTracker::with_secret([1, 2, 3, 4]);
        let hash = [0xABu8; 20];
        let source: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let changed_port: SocketAddr = "10.0.0.1:49152".parse().unwrap();
        let changed_ip: SocketAddr = "10.0.0.2:6881".parse().unwrap();
        let token = tt.generate_token(&hash, &source);

        assert!(tt.validate_token(&token, &hash, &changed_port));
        assert!(!tt.validate_token(&token, &hash, &changed_ip));
    }

    #[test]
    fn caller_can_provide_full_width_secret() {
        let tracker = TokenTracker::with_secret_bytes([7; SECRET_SIZE]);
        let hash = [0x42; 20];
        let addr: SocketAddr = "192.0.2.1:6881".parse().unwrap();
        let token = tracker.generate_token(&hash, &addr);

        assert!(tracker.validate_token(&token, &hash, &addr));
        assert_ne!(
            token,
            TokenTracker::with_secret([7; 4]).generate_token(&hash, &addr)
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
    fn configured_rotation_interval_controls_token_rotation() {
        let mut tt = TokenTracker::new();
        tt.last_rotation = Instant::now() - Duration::from_secs(5);

        assert!(!tt.maybe_rotate_after(Duration::from_secs(6)));
        assert!(tt.maybe_rotate_after(Duration::from_secs(4)));
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

    #[test]
    fn test_opaque_token_bytes_are_validated_without_utf8() {
        let tt = TokenTracker::with_secret([1, 2, 3, 4]);
        let hash = [0x55u8; 20];
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let generated = tt.generate_token(&hash, &addr);
        let mut token = generated.into_bytes();
        token[0] = 0xff;
        assert!(!tt.validate_token_bytes(&token, &hash, &addr));
        assert!(tt.validate_token_bytes(tt.generate_token(&hash, &addr).as_bytes(), &hash, &addr));
    }
}
