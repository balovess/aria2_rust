//! Integration tests for the authentication challenge handler.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use base64::Engine;

    use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
    use crate::http::auth_challenge_handler::{AuthChallengeResult, handle_auth_challenge};
    use crate::http::request_response::HttpMethod;
    use crate::http::skip_response::{AuthScheme, HttpAuthChallenge};

    fn make_url(url_str: &str) -> url::Url {
        url::Url::parse(url_str).expect("test URL must be valid")
    }

    fn default_auth_opts() -> AuthResolveOptions {
        AuthResolveOptions::default()
    }

    fn auth_opts_with_challenge() -> AuthResolveOptions {
        AuthResolveOptions {
            http_auth_challenge: true,
            ..AuthResolveOptions::default()
        }
    }

    fn auth_opts_with_cli_creds() -> AuthResolveOptions {
        AuthResolveOptions {
            http_user: Some("testuser".to_string()),
            http_passwd: Some("testpass".to_string()),
            ..AuthResolveOptions::default()
        }
    }

    // --- Basic Auth Tests ---

    #[test]
    fn test_basic_auth_with_cli_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "TestRealm".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        // Need both http_auth_challenge=true AND CLI credentials
        let opts = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("testuser".to_string()),
            http_passwd: Some("testpass".to_string()),
            ..AuthResolveOptions::default()
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                assert!(authorization_header.starts_with("Basic "));
                assert!(!is_proxy);
                // Verify the Base64-decoded value is "testuser:testpass"
                let encoded = authorization_header
                    .strip_prefix("Basic ")
                    .expect("must have Basic prefix");
                let decoded = String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .expect("must be valid base64"),
                )
                .expect("must be valid UTF-8");
                assert_eq!(decoded, "testuser:testpass");
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_with_url_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://user:pass@example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        let opts = auth_opts_with_challenge();

        // Simulate real flow: first resolve credentials from the URL,
        // which populates the BasicCred cache (this is what happens when
        // the initial request is built in the C++ flow).
        let _initial_auth = factory.resolve(&url, false, &opts);

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                ..
            } => {
                assert!(authorization_header.starts_with("Basic "));
                let encoded = authorization_header
                    .strip_prefix("Basic ")
                    .expect("must have Basic prefix");
                let decoded = String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .expect("must be valid base64"),
                )
                .expect("must be valid UTF-8");
                assert_eq!(decoded, "user:pass");
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_no_credentials_fails() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        // No CLI creds, no URL creds, no Netrc
        let opts = auth_opts_with_challenge();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_already_used_prevents_loop() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://user:pass@example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        let opts = auth_opts_with_challenge();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            true, // Already tried auth
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { message, .. } => {
                assert!(message.contains("already tried"));
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    #[test]
    fn test_proxy_auth_challenge() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "ProxyRealm".to_string(),
            is_proxy: true,
            digest_challenge: None,
        };
        let opts = AuthResolveOptions {
            http_auth_challenge: true,
            proxy_user: Some("proxyuser".to_string()),
            proxy_passwd: Some("proxypass".to_string()),
            ..AuthResolveOptions::default()
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth { is_proxy, .. } => {
                assert!(is_proxy);
            }
            _ => panic!(
                "Expected RetryWithAuth with is_proxy=true, got {:?}",
                result
            ),
        }
    }

    // --- Digest Auth Tests ---

    #[test]
    fn test_digest_auth_with_cli_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected/data");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Digest,
            realm: "Downloads".to_string(),
            is_proxy: false,
            digest_challenge: Some(
                crate::http::digest_auth::DigestAuthChallenge::parse(
                    r#"Digest realm="Downloads", nonce="abc123def", qop="auth", algorithm="MD5""#,
                )
                .expect("digest challenge must parse"),
            ),
        };
        let opts = auth_opts_with_cli_creds();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                assert!(authorization_header.starts_with("Digest "));
                assert!(authorization_header.contains(r#"username="testuser""#));
                assert!(authorization_header.contains(r#"realm="Downloads""#));
                assert!(authorization_header.contains(r#"nonce="abc123def""#));
                assert!(authorization_header.contains(r#"uri="/protected/data""#));
                assert!(authorization_header.contains("response="));
                assert!(!is_proxy);
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_digest_auth_no_credentials_fails() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Digest,
            realm: "Secure".to_string(),
            is_proxy: false,
            digest_challenge: Some(
                crate::http::digest_auth::DigestAuthChallenge::parse(
                    r#"Digest realm="Secure", nonce="xyz""#,
                )
                .expect("digest challenge must parse"),
            ),
        };
        let opts = default_auth_opts();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    // --- Unsupported Scheme Tests ---

    #[test]
    fn test_ntlm_unsupported() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Ntlm,
            realm: String::new(),
            is_proxy: false,
            digest_challenge: None,
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &default_auth_opts(),
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::UnsupportedScheme {
                scheme,
                status_code,
            } => {
                assert_eq!(scheme, "NTLM");
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected UnsupportedScheme, got {:?}", result),
        }
    }

    #[test]
    fn test_negotiate_unsupported() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Negotiate,
            realm: String::new(),
            is_proxy: false,
            digest_challenge: None,
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &default_auth_opts(),
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::UnsupportedScheme { scheme, .. } => {
                assert_eq!(scheme, "Negotiate");
            }
            _ => panic!("Expected UnsupportedScheme, got {:?}", result),
        }
    }
}
