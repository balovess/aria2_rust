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
        self.hash(input.as_bytes())
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
            let body_hash = self.hash(entity_body.unwrap_or_default());
            format!("{}:{}:{}", method, uri, body_hash)
        } else {
            format!("{}:{}", method, uri)
        };
        self.hash(input.as_bytes())
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

        self.hash(format!("{ha1}:{kd_input}").as_bytes())
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

        // Generate cnonce (client nonce) - in production, use crypto-random
        let cnonce = format!("{:016x}", rand::random::<u64>());

        // Increment and get nonce count
        let nc_raw = self.nc_count.fetch_add(1, Ordering::SeqCst);

        let protocol_challenge = aria2_protocol::http::digest::DigestChallenge {
            realm: challenge.realm.clone(),
            nonce: nonce.to_owned(),
            qop: challenge.qop.clone(),
            algorithm: self.algorithm.to_string(),
            opaque: challenge.opaque.clone(),
            stale: challenge.stale,
        };
        let response = aria2_protocol::http::digest::DigestResponse::compute_with_body_and_cnonce(
            self.username.expose_secret(),
            self.password.expose_secret(),
            method,
            uri,
            &protocol_challenge,
            nc_raw,
            &cnonce,
            entity_body,
        );
        Ok(response.to_header_value())
    }

    /// Hashes a string using the configured algorithm.
    pub(crate) fn hash(&self, input: &[u8]) -> String {
        aria2_protocol::http::digest::hash_value(&self.algorithm.to_string(), input)
            .expect("configured Digest algorithm is supported")
    }
}
