//! QOP handling, nonce counting, and digest computation (RFC 7616)
//!
//! Implements the core cryptographic operations for Digest authentication:
//! - HA1 computation: H(username:realm:password)
//! - HA2 computation: H(method:uri) or H(method:uri:H(entity-body))
//! - Response computation: KD(HA1, nonce:nc:cnonce:qop:HA2)
//! - Full Authorization header assembly with QOP parameters

use std::sync::atomic::Ordering;

use crate::error::{Aria2Error, Result};

use super::{AuthChallenge, DigestAlgorithm, DigestAuthProvider};

impl DigestAuthProvider {
    /// Computes HA1 = H(username:realm:password).
    ///
    /// This is the first step in Digest authentication response calculation.
    /// The hash algorithm depends on the provider's configured algorithm.
    ///
    /// # Arguments
    /// * `realm` - The realm from the authentication challenge
    ///
    /// # Returns
    /// Hex-encoded hash string
    pub fn compute_ha1(&self, realm: &str) -> String {
        let input = format!(
            "{}:{}:{}",
            self.username.expose_secret(),
            realm,
            self.password.expose_secret()
        );
        self.hash(&input)
    }

    /// Computes HA2 = H(method:uri) or H(method:uri:H(entity-body)).
    ///
    /// When qop is "auth-int", includes the hash of the entity body.
    ///
    /// # Arguments
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `uri` - Request URI
    /// * `qop` - Quality of protection mode
    /// * `entity_body` - Optional entity body for auth-int mode
    ///
    /// # Returns
    /// Hex-encoded hash string
    pub fn compute_ha2(
        &self,
        method: &str,
        uri: &str,
        qop: Option<&str>,
        entity_body: Option<&[u8]>,
    ) -> String {
        let input = if qop == Some("auth-int") {
            let body_hash = match entity_body {
                Some(body) => self.hash_from_bytes(body),
                None => self.hash(""),
            };
            format!("{}:{}:{}", method, uri, body_hash)
        } else {
            format!("{}:{}", method, uri)
        };
        self.hash(&input)
    }

    /// Computes the final response = KD(HA1, nonce:nc:cnonce:qop:HA2).
    ///
    /// KD(secret, data) = H(concat(secret, ":", data))
    ///
    /// # Arguments
    /// * `ha1` - Pre-computed HA1 value
    /// * `nonce` - Server-provided nonce
    /// * `nonce_count` - Hex-encoded 8-digit nonce count
    /// * `cnonce` - Client-generated nonce
    /// * `qop` - Quality of protection mode
    /// * `ha2` - Pre-computed HA2 value
    ///
    /// # Returns
    /// Hex-encoded response string
    pub fn compute_response(
        &self,
        ha1: &str,
        nonce: &str,
        nonce_count: &str,
        cnonce: &str,
        qop: Option<&str>,
        ha2: &str,
    ) -> String {
        let kd_input = match qop {
            Some(q) => format!("{}:{}:{}:{}:{}", nonce, nonce_count, cnonce, q, ha2),
            None => format!("{}:{}", nonce, ha2),
        };

        self.hash_kd(ha1, &kd_input)
    }

    /// Builds a complete Digest Authorization header value.
    ///
    /// Constructs all necessary components including:
    /// - Username, realm, nonce, uri
    /// - Algorithm specification
    /// - Response hash
    /// - Optional: qop, nc, cnonce, opaque
    ///
    /// # Arguments
    /// * `challenge` - Server authentication challenge
    /// * `method` - HTTP method for the request
    /// * `uri` - Request URI
    /// * `entity_body` - Optional entity body for auth-int
    ///
    /// # Returns
    /// Complete Authorization header value starting with "Digest "
    pub fn build_authorization_header_with_method(
        &self,
        challenge: &AuthChallenge,
        method: &str,
        uri: &str,
        entity_body: Option<&[u8]>,
    ) -> Result<String> {
        let nonce = challenge
            .nonce
            .as_deref()
            .ok_or_else(|| Aria2Error::Parse("Missing nonce in Digest challenge".to_string()))?;

        // Compute HA1
        let ha1 = self.compute_ha1(&challenge.realm);

        // Generate cnonce (client nonce) - in production, use crypto-random
        let cnonce = format!("{:016x}", rand::random::<u64>());

        // Increment and get nonce count
        let nc_raw = self.nc_count.fetch_add(1, Ordering::SeqCst);
        let nc = format!("{:08x}", nc_raw);

        // Determine QoP
        let qop = challenge.qop.as_deref();

        // Compute HA2
        let ha2 = self.compute_ha2(method, uri, qop, entity_body);

        // Compute response
        let response = self.compute_response(&ha1, nonce, &nc, &cnonce, qop, &ha2);

        // Build the authorization header
        let mut parts = vec![
            format!("username=\"{}\"", self.username.expose_secret()),
            format!("realm=\"{}\"", challenge.realm),
            format!("nonce=\"{}\"", nonce),
            format!("uri=\"{}\"", uri),
            format!("algorithm={}", self.algorithm),
            format!("response=\"{}\"", response),
        ];

        if let Some(q) = qop {
            parts.push(format!("qop={}", q));
            parts.push(format!("nc={}", nc));
            parts.push(format!("cnonce=\"{}\"", cnonce));
        }

        if let Some(ref opaque) = challenge.opaque {
            parts.push(format!("opaque=\"{}\"", opaque));
        }

        Ok(format!("Digest {}", parts.join(", ")))
    }

    /// Hashes a string using the configured algorithm.
    pub(crate) fn hash(&self, input: &str) -> String {
        self.hash_from_bytes(input.as_bytes())
    }

    /// Hashes bytes using the configured algorithm.
    pub(crate) fn hash_from_bytes(&self, input: &[u8]) -> String {
        match self.algorithm {
            DigestAlgorithm::Md5 => {
                use md5::Digest;
                let mut hasher = md5::Md5::new();
                hasher.update(input);
                let digest = hasher.finalize();
                format!("{:x}", digest)
            }
            DigestAlgorithm::Sha256 => {
                use sha2::{Digest as _, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(input);
                let result = hasher.finalize();
                hex::encode(result)
            }
            DigestAlgorithm::Sha512_256 => {
                use sha2::{Digest as _, Sha512_256};
                let mut hasher = Sha512_256::new();
                hasher.update(input);
                let result = hasher.finalize();
                hex::encode(result)
            }
        }
    }

    /// Computes KD(secret, data) = H(secret ":" data)
    pub(crate) fn hash_kd(&self, secret: &str, data: &str) -> String {
        let kd_input = format!("{}:{}", secret, data);
        self.hash(&kd_input)
    }
}
