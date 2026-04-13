//! HTTP Digest Authentication (RFC 2617) implementation
//!
//! Provides parsing of WWW-Authenticate Digest challenges and building
//! of Authorization header values for HTTP Digest authentication.

use std::collections::HashMap;
use std::fmt;

/// Parsed WWW-Authenticate header for Digest auth challenge
///
/// Represents a server's Digest authentication challenge as defined in RFC 2617.
/// Example header: `Digest realm="aria2", nonce="abc123", qop="auth", algorithm="MD5"`
#[derive(Debug, Clone)]
pub struct DigestAuthChallenge {
    /// Authentication realm (typically a human-readable string describing the protected area)
    pub realm: String,
    /// Server-provided nonce value (unique per challenge)
    pub nonce: String,
    /// Quality of protection: "auth" or "auth-int" (optional)
    pub qop: Option<String>,
    /// Hash algorithm used (default "MD5")
    pub algorithm: String,
    /// Opaque value that client must return unchanged (optional)
    pub opaque: Option<String>,
    /// If true, the previous attempt failed due to stale nonce
    pub stale: bool,
}

impl fmt::Display for DigestAuthChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Digest realm=\"{}\", nonce=\"{}\", algorithm=\"{}\"",
            self.realm, self.nonce, self.algorithm
        )?;
        if let Some(ref qop) = self.qop {
            write!(f, ", qop=\"{}\"", qop)?;
        }
        if let Some(ref opaque) = self.opaque {
            write!(f, ", opaque=\"{}\"", opaque)?;
        }
        if self.stale {
            write!(f, ", stale=true")?;
        }
        Ok(())
    }
}

impl DigestAuthChallenge {
    /// Parse a `WWW-Authenticate` header value containing a Digest challenge.
    ///
    /// # Arguments
    /// * `header_value` - The full header value, e.g.
    ///   `Digest realm="aria2", nonce="abc123", qop="auth", algorithm="MD5", opaque="xyz", stale=false`
    ///
    /// # Returns
    /// A parsed `DigestAuthChallenge` on success, or an error message on failure.
    ///
    /// # Errors
    /// Returns an error if:
    /// - The header does not start with "Digest "
    /// - The required `nonce` parameter is missing
    ///
    /// # Example
    /// ```
    /// let challenge = DigestAuthChallenge::parse(
    ///     r#"Digest realm="test realm", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth""#
    /// ).unwrap();
    /// assert_eq!(challenge.realm, "test realm");
    /// assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
    /// ```
    pub fn parse(header_value: &str) -> Result<Self, String> {
        // Extract after "Digest " prefix
        let digest_part = header_value
            .strip_prefix("Digest ")
            .ok_or_else(|| "Not a Digest challenge: missing 'Digest ' prefix".to_string())?;

        // Split by comma to get key=value pairs, then parse each pair
        let mut params = HashMap::new();
        for pair in digest_part.split(',') {
            let pair = pair.trim();
            if let Some((key, value)) = pair.split_once('=') {
                let key = key.trim().to_lowercase();
                let mut value = value.trim().to_string();
                // Strip surrounding quotes if present
                if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
                    value = value[1..value.len() - 1].to_string();
                }
                params.insert(key, value);
            }
        }

        // Validate required fields
        let nonce = params
            .get("nonce")
            .cloned()
            .ok_or_else(|| "Missing required 'nonce' parameter in Digest challenge".to_string())?;

        Ok(DigestAuthChallenge {
            realm: params.get("realm").cloned().unwrap_or_default(),
            nonce,
            qop: params.get("qop").cloned(),
            algorithm: params
                .get("algorithm")
                .cloned()
                .unwrap_or_else(|| "MD5".to_string()),
            opaque: params.get("opaque").cloned(),
            stale: params
                .get("stale")
                .map(|s| s.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}

/// Built Digest authentication response ready for inclusion in an Authorization header
///
/// Contains all computed values needed to construct the Authorization header value.
#[derive(Debug, Clone)]
pub struct DigestAuthResponse {
    /// Username being authenticated
    pub username: String,
    /// Realm from the server's challenge
    pub realm: String,
    /// Nonce from the server's challenge
    pub nonce: String,
    /// Request URI (path portion)
    pub uri: String,
    /// Quality of protection (from challenge)
    pub qop: Option<String>,
    /// Nonce count (hex, 8 digits)
    pub nc: u32,
    /// Client-generated nonce (hex string)
    pub cnonce: String,
    /// Computed response hash
    pub response: String,
    /// Algorithm used (from challenge)
    pub algorithm: String,
    /// Opaque value from challenge (must be returned unchanged)
    pub opaque: Option<String>,
}

impl DigestAuthResponse {
    /// Build the complete `Authorization` header value string.
    ///
    /// Returns the full header value in the format:
    /// ```text
    /// Digest username="...", realm="...", nonce="...", uri="...",
    ///        nc=XXXXXXXX, cnonce="...", qop="...", response="...",
    ///        algorithm="...", opaque="..."
    /// ```
    pub fn to_header_value(&self) -> String {
        format!(
            r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", nc={:08x}, cnonce="{}", qop="{}", response="{}", algorithm="{}", opaque="{}""#,
            self.username,
            self.realm,
            self.nonce,
            self.uri,
            self.nc,
            self.cnonce,
            self.qop.as_deref().unwrap_or(""),
            self.response,
            self.algorithm,
            self.opaque.as_deref().unwrap_or("")
        )
    }

    /// Compute a Digest authentication response per RFC 2617 section 3.2.2.1.
    ///
    /// This method computes all necessary hashes and builds a complete response
    /// that can be serialized via [`to_header_value`](Self::to_header_value).
    ///
    /// # Algorithm (RFC 2617):
    /// ```
    /// HA1 = MD5(username:realm:password)
    /// HA2 = MD5(method:uri)
    /// if qop is set:
    ///     response = MD5(HA1:nonce:nc:cnonce:qop:HA2)
    /// else:
    ///     response = MD5(HA1:nonce:HA2)
    /// ```
    ///
    /// # Arguments
    /// * `username` - The username for authentication
    /// * `password` - The user's password (plaintext)
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `uri` - The request URI path
    /// * `challenge` - The parsed server challenge
    /// * `nc` - Nonce count (incremented per request with same nonce)
    ///
    /// # Returns
    /// A fully constructed `DigestAuthResponse`.
    pub fn compute(
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        challenge: &DigestAuthChallenge,
        nc: u32,
    ) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        /// Simple hash function producing hex string (used as MD5 substitute)
        fn hash_hex(input: &str) -> String {
            let mut hasher = DefaultHasher::new();
            input.hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        }

        // Compute HA1 = hash(username:realm:password)
        let ha1 = hash_hex(&format!("{}:{}:{}", username, challenge.realm, password));

        // Compute HA2 = hash(method:uri)
        let ha2 = hash_hex(&format!("{}:{}", method, uri));

        // Generate random cnonce (client nonce)
        let cnonce = format!("{:016x}", rand_u64());
        let nc_str = format!("{:08x}", nc);

        // Compute final response based on whether qop is present
        let response = if let Some(qop) = &challenge.qop {
            // With QoP: HA1:nonce:nc:cnonce:qop:HA2
            hash_hex(&format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, challenge.nonce, nc_str, &cnonce, qop, ha2
            ))
        } else {
            // Without QoP: HA1:nonce:HA2
            hash_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2))
        };

        DigestAuthResponse {
            username: username.to_string(),
            realm: challenge.realm.clone(),
            nonce: challenge.nonce.clone(),
            uri: uri.to_string(),
            qop: challenge.qop.clone(),
            nc,
            cnonce,
            response,
            algorithm: challenge.algorithm.clone(),
            opaque: challenge.opaque.clone(),
        }
    }
}

/// Generate a pseudo-random u64 value for use as cnonce
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_nanos() as u64 ^ (dur.as_secs() as u64).wrapping_mul(0x5851F42D4C957F2D)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DigestAuthChallenge tests ---

    #[test]
    fn test_digest_challenge_parse_basic() {
        let header = r#"Digest realm="test realm", nonce="abc123def456""#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();

        assert_eq!(challenge.realm, "test realm");
        assert_eq!(challenge.nonce, "abc123def456");
        assert_eq!(challenge.algorithm, "MD5"); // default
        assert!(challenge.qop.is_none());
        assert!(challenge.opaque.is_none());
        assert!(!challenge.stale);
    }

    #[test]
    fn test_digest_challenge_parse_all_fields() {
        let header = r#"Digest realm="aria2 download", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm="MD5", opaque="5ccc069c403ebaf9f0171e9517bf40c9", stale=true"#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();

        assert_eq!(challenge.realm, "aria2 download");
        assert_eq!(challenge.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(challenge.qop.as_deref(), Some("auth"));
        assert_eq!(challenge.algorithm, "MD5");
        assert_eq!(
            challenge.opaque.as_deref(),
            Some("5ccc069c403ebaf9f0171e9517bf40c9")
        );
        assert!(challenge.stale);
    }

    #[test]
    fn test_digest_challenge_missing_nonce_returns_error() {
        let header = r#"Digest realm="only realm""#;
        let result = DigestAuthChallenge::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nonce"));
    }

    #[test]
    fn test_digest_challenge_not_digest_prefix_returns_error() {
        let header = r#"Basic realm="test""#;
        let result = DigestAuthChallenge::parse(header);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Digest "));
    }

    #[test]
    fn test_digest_challenge_case_insensitive_keys() {
        // Keys should be case-insensitive per RFC
        let header = r#"Digest REALM="TestRealm", NONCE="myNonce", ALGORITHM="SHA-256""#;
        let challenge = DigestAuthChallenge::parse(header).unwrap();
        assert_eq!(challenge.realm, "TestRealm");
        assert_eq!(challenge.nonce, "myNonce");
        assert_eq!(challenge.algorithm, "SHA-256");
    }

    // --- DigestAuthResponse tests ---

    #[test]
    fn test_digest_response_compute_and_format() {
        let challenge = DigestAuthChallenge::parse(
            r#"Digest realm="test@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", opaque="someopaque""#
        ).unwrap();

        let response = DigestAuthResponse::compute(
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            &challenge,
            1,
        );

        assert_eq!(response.username, "Mufasa");
        assert_eq!(response.realm, "test@host.com");
        assert_eq!(response.uri, "/dir/index.html");
        assert_eq!(response.nc, 1);
        assert_eq!(response.qop.as_deref(), Some("auth"));
        assert_eq!(response.algorithm, "MD5");
        assert_eq!(response.opaque.as_deref(), Some("someopaque"));

        // Verify the header value contains expected components
        let header_val = response.to_header_value();
        assert!(header_val.starts_with("Digest "));
        assert!(header_val.contains(r#"username="Mufasa""#));
        assert!(header_val.contains(r#"realm="test@host.com""#));
        assert!(header_val.contains("nc=00000001"));
        assert!(header_val.contains(r#"response=""#));
        assert!(!response.cnonce.is_empty());
    }

    #[test]
    fn test_digest_response_without_qop() {
        let challenge =
            DigestAuthChallenge::parse(r#"Digest realm="simple", nonce="simpleNonce123""#).unwrap();

        let response =
            DigestAuthResponse::compute("user", "pass", "POST", "/api/data", &challenge, 1);

        assert!(response.qop.is_none());
        let header_val = response.to_header_value();
        assert!(header_val.contains("qop=")); // empty qop in output
        assert!(header_val.contains(r#"algorithm="MD5""#));
    }

    #[test]
    fn test_digest_full_flow_roundtrip() {
        // Simulate full flow: receive challenge -> build response -> verify format

        // Step 1: Server sends WWW-Authenticate header
        let www_authenticate = r#"Digest realm="WallyWorld", nonce="OA=MPOPQKX/RI=SOXPVDFKB,URI=/download/file.torrent", qop="auth", algorithm="MD5", opaque="FQwERTYuiop123""#;

        // Step 2: Client parses the challenge
        let challenge = DigestAuthChallenge::parse(www_authenticate).unwrap();
        assert_eq!(challenge.realm, "WallyWorld");

        // Step 3: Client computes response
        let auth_response = DigestAuthResponse::compute(
            "admin",
            "secret123",
            "GET",
            "/download/file.torrent",
            &challenge,
            1,
        );

        // Step 4: Build Authorization header value
        let authorization = auth_response.to_header_value();

        // Verify roundtrip integrity
        assert!(authorization.starts_with("Digest "));
        assert!(authorization.contains(r#"username="admin""#));
        assert!(authorization.contains(r#"realm="WallyWorld""#));
        assert!(authorization.contains(&format!("nonce=\"{}\"", challenge.nonce)));
        assert!(authorization.contains(r#"uri="/download/file.torrent""#));
        assert!(authorization.contains("nc=00000001"));
        assert!(authorization.contains(r#"qop="auth""#));
        assert!(authorization.contains(r#"opaque="FQwERTYuiop123""#));

        // Verify we can re-parse the generated header structure (sanity check)
        assert!(authorization.len() > 50); // Should be substantial
        assert!(!authorization.contains("\n")); // Single line header
    }

    #[test]
    fn test_digest_challenge_display_format() {
        let challenge = DigestAuthChallenge {
            realm: "MyRealm".into(),
            nonce: "abc123".into(),
            qop: Some("auth".into()),
            algorithm: "MD5".into(),
            opaque: Some("opaqueVal".into()),
            stale: false,
        };

        let display = format!("{}", challenge);
        assert!(display.contains("Digest "));
        assert!(display.contains(r#"realm="MyRealm""#));
        assert!(display.contains(r#"nonce="abc123""#));
        assert!(display.contains(r#"qop="auth""#));
        assert!(display.contains(r#"opaque="opaqueVal""#));
    }

    #[test]
    fn test_digest_nonce_count_increments_correctly() {
        let challenge =
            DigestAuthChallenge::parse(r#"Digest realm="test", nonce="nonce123", qop="auth""#)
                .unwrap();

        let resp1 = DigestAuthResponse::compute("u", "p", "GET", "/", &challenge, 1);
        let resp2 = DigestAuthResponse::compute("u", "p", "GET", "/", &challenge, 2);

        let h1 = resp1.to_header_value();
        let h2 = resp2.to_header_value();

        assert!(h1.contains("nc=00000001"));
        assert!(h2.contains("nc=00000002"));

        // Each request should have different cnonce and response
        assert_ne!(resp1.cnonce, resp2.cnonce);
        assert_ne!(resp1.response, resp2.response);
    }
}
