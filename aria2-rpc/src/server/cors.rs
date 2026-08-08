//! CORS configuration for the RPC HTTP server.

#[derive(Debug, Clone)]
pub struct CorsConfig {
    pub allow_origin: String,
    pub allow_methods: String,
    pub allow_headers: String,
    pub allow_credentials: bool,
    /// Parsed list of allowed origins for efficient lookup
    allowed_origins: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self::with_allowed_origins(vec![crate::constants::CORS_DEFAULT_ORIGIN.to_string()])
    }
}

impl CorsConfig {
    /// Create a new CorsConfig from a comma-separated list of allowed origins
    ///
    /// Special value "*" allows all origins (wildcard mode).
    /// Multiple origins can be specified as "http://localhost:8080,https://example.com"
    pub fn with_allowed_origins(origins: Vec<String>) -> Self {
        let origins = origins
            .into_iter()
            .map(|origin| origin.trim().to_string())
            .filter(|origin| !origin.is_empty())
            .collect::<Vec<_>>();
        let allow_origin = if origins.len() == 1 && origins[0] == "*" {
            "*".to_string()
        } else {
            origins.join(", ")
        };

        Self {
            allow_origin: allow_origin.clone(),
            allow_methods: aria2_core::constants::CORS_ALLOW_METHODS.to_string(),
            allow_headers: aria2_core::constants::CORS_ALLOW_HEADERS.to_string(),
            allow_credentials: false,
            allowed_origins: origins,
        }
    }

    /// Create CorsConfig from an option value string (comma-separated origins)
    pub fn from_option_value(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed == crate::constants::CORS_DEFAULT_ORIGIN {
            return Self::default();
        }

        let origins = trimmed
            .split(',')
            .map(str::trim)
            .map(str::to_string)
            .collect();

        Self::with_allowed_origins(origins)
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        let origin = origin.into();
        self.allowed_origins = origin
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(str::to_string)
            .collect();
        self.allow_origin = origin;
        self
    }

    pub fn with_methods(mut self, methods: impl Into<String>) -> Self {
        self.allow_methods = methods.into();
        self
    }

    pub fn with_headers(mut self, headers: impl Into<String>) -> Self {
        self.allow_headers = headers.into();
        self
    }

    pub fn with_credentials(mut self) -> Self {
        self.allow_credentials = true;
        self
    }

    /// Check if a given origin is allowed by this CORS configuration
    ///
    /// Returns true if:
    /// - Wildcard mode is enabled ("*" is in allowed_origins)
    /// - The origin exactly matches one of the allowed origins
    /// - No origin header is provided (browser navigation / non-CORS request)
    pub fn allows_origin(&self, origin: Option<&str>) -> bool {
        // Wildcard allows everything
        if self
            .allowed_origins
            .iter()
            .any(|s| s == crate::constants::CORS_DEFAULT_ORIGIN)
        {
            return true;
        }

        match origin {
            Some(o) => self.allowed_origins.iter().any(|allowed| allowed == o),
            None => true, // No Origin header = allow (browser navigation)
        }
    }

    pub(crate) fn is_wildcard(&self) -> bool {
        self.allowed_origins
            .iter()
            .any(|origin| origin == crate::constants::CORS_DEFAULT_ORIGIN)
    }

    pub(crate) fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// Generate CORS headers for a response
    ///
    /// Returns None if the origin is not allowed.
    /// Returns Some(headers) with appropriate CORS headers if allowed.
    pub fn headers_for_origin(&self, origin: Option<&str>) -> Option<Vec<(&'static str, String)>> {
        let origin_str = match origin {
            Some(o) if self.allows_origin(Some(o)) => {
                if self.is_wildcard() && !self.allow_credentials {
                    crate::constants::CORS_DEFAULT_ORIGIN.to_string()
                } else {
                    o.to_string()
                }
            }
            None if self.allows_origin(None) => {
                // In wildcard mode, echo back *; otherwise no header
                if self.is_wildcard() {
                    crate::constants::CORS_DEFAULT_ORIGIN.to_string()
                } else {
                    return Some(vec![]); // Allow but don't set specific origin
                }
            }
            _ => return None, // Origin not allowed
        };

        Some(vec![
            ("Access-Control-Allow-Origin", origin_str),
            ("Access-Control-Allow-Methods", self.allow_methods.clone()),
            ("Access-Control-Allow-Headers", self.allow_headers.clone()),
            (
                "Access-Control-Max-Age",
                aria2_core::constants::CORS_MAX_AGE.to_string(),
            ),
        ])
    }

    /// Get headers as static str pairs (for non-origin-specific responses)
    pub fn to_headers(&self) -> Vec<(&str, &str)> {
        vec![
            ("Access-Control-Allow-Origin", &self.allow_origin),
            ("Access-Control-Allow-Methods", &self.allow_methods),
            ("Access-Control-Allow-Headers", &self.allow_headers),
            (
                "Access-Control-Max-Age",
                aria2_core::constants::CORS_MAX_AGE,
            ),
        ]
    }

    /// Handle OPTIONS preflight request - returns true if preflight should be allowed
    pub fn handle_preflight(&self, origin: Option<&str>) -> bool {
        self.allows_origin(origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cors_config_default() {
        let cors = CorsConfig::default();
        let headers = cors.to_headers();
        assert!(
            headers
                .iter()
                .any(|(k, _)| k == &"Access-Control-Allow-Origin")
        );
    }

    #[test]
    fn test_cors_wildcard_allows_any_origin() {
        let cors = CorsConfig::default(); // Default is wildcard "*"

        // Wildcard should allow any origin
        assert!(cors.allows_origin(Some("http://localhost:8080")));
        assert!(cors.allows_origin(Some("https://example.com")));
        assert!(cors.allows_origin(Some("http://192.168.1.1:3000")));
        assert!(cors.allows_origin(None)); // No origin header
    }

    #[test]
    fn test_cors_specific_domain_blocks_others() {
        let cors = CorsConfig::from_option_value("http://localhost:8080,https://example.com");

        // Allowed origins should pass
        assert!(
            cors.allows_origin(Some("http://localhost:8080")),
            "Should allow exact match for localhost"
        );
        assert!(
            cors.allows_origin(Some("https://example.com")),
            "Should allow exact match for example.com"
        );

        // Non-allowed origins should be blocked
        assert!(
            !cors.allows_origin(Some("http://evil.com")),
            "Should block non-listed origin"
        );
        assert!(
            !cors.allows_origin(Some("http://localhost:8081")),
            "Should block different port"
        );
        assert!(
            !cors.allows_origin(Some("http://localhost:8080/extra")),
            "Should block origin with path (strict matching)"
        );

        // No origin header should still be allowed
        assert!(
            cors.allows_origin(None),
            "No origin header should be allowed"
        );
    }

    #[test]
    fn test_cors_preflight_returns_true_for_allowed() {
        let cors = CorsConfig::from_option_value("http://localhost:8080");

        // Preflight should succeed for allowed origin
        assert!(cors.handle_preflight(Some("http://localhost:8080")));

        // Preflight should fail for disallowed origin
        assert!(!cors.handle_preflight(Some("http://evil.com")));

        // No origin - preflight should succeed
        assert!(cors.handle_preflight(None));
    }

    #[test]
    fn test_cors_from_option_value_parsing() {
        // Test wildcard
        let cors_wildcard = CorsConfig::from_option_value("*");
        assert!(cors_wildcard.allows_origin(Some("anything")));
        assert_eq!(cors_wildcard.allow_origin, "*");

        // Test empty string defaults to wildcard
        let cors_empty = CorsConfig::from_option_value("");
        assert!(cors_empty.allows_origin(Some("anything")));

        // Test multiple origins
        let cors_multi =
            CorsConfig::from_option_value("http://a.com, https://b.com, http://c.com:9090");
        assert!(cors_multi.allows_origin(Some("http://a.com")));
        assert!(cors_multi.allows_origin(Some("https://b.com")));
        assert!(cors_multi.allows_origin(Some("http://c.com:9090")));
        assert!(!cors_multi.allows_origin(Some("http://d.com")));

        // Test with whitespace handling
        let cors_spaces = CorsConfig::from_option_value("  http://a.com , https://b.com  ");
        assert!(cors_spaces.allows_origin(Some("http://a.com")));
        assert!(cors_spaces.allows_origin(Some("https://b.com")));
    }

    #[test]
    fn test_cors_headers_for_origin() {
        let cors = CorsConfig::from_option_value("http://localhost:8080");

        // Allowed origin should produce headers
        let headers = cors.headers_for_origin(Some("http://localhost:8080"));
        assert!(
            headers.is_some(),
            "Should return headers for allowed origin"
        );
        let headers = headers.unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, _)| *k == "Access-Control-Allow-Origin"),
            "Should contain Allow-Origin header"
        );

        // Disallowed origin should return None
        let blocked = cors.headers_for_origin(Some("http://evil.com"));
        assert!(blocked.is_none(), "Should return None for blocked origin");
    }

    #[test]
    fn test_cors_with_allowed_origins_constructor() {
        let cors = CorsConfig::with_allowed_origins(vec![
            "https://api.example.com".to_string(),
            "http://localhost:3000".to_string(),
        ]);

        assert!(cors.allows_origin(Some("https://api.example.com")));
        assert!(cors.allows_origin(Some("http://localhost:3000")));
        assert!(!cors.allows_origin(Some("http://other.com")));
    }
}
