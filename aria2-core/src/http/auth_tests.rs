use super::auth::netrc::NetrcError;
use super::auth::*;
use url::Url;

#[test]
fn test_auth_config_new_returns_none_for_empty_user() {
    assert!(AuthConfig::new(String::new(), String::new()).is_none());
}

#[test]
fn test_auth_config_auth_text() {
    let ac = AuthConfig::new("admin".into(), "s3cret".into()).unwrap();
    assert_eq!(ac.auth_text(), "admin:s3cret");
    assert_eq!(ac.user(), "admin");
    assert_eq!(ac.password(), "s3cret");
}

#[test]
fn test_auth_config_display() {
    let ac = AuthConfig::new("u".into(), "p".into()).unwrap();
    assert_eq!(format!("{}", ac), "u:p");
}

#[test]
fn test_basic_cred_path_normalisation() {
    let bc = BasicCred::new("u".into(), "p".into(), "h".into(), 80, "/dir".into(), false);
    assert_eq!(bc.path, "/dir/");
}

#[test]
fn test_basic_cred_equality() {
    let a = BasicCred::new("u1".into(), "p1".into(), "h".into(), 80, "/".into(), false);
    let b = BasicCred::new("u2".into(), "p2".into(), "h".into(), 80, "/".into(), true);
    // Equality is by host/port/path only
    assert_eq!(a, b);
}

#[test]
fn test_basic_cred_ordering() {
    let a = BasicCred::new("".into(), "".into(), "a".into(), 80, "/".into(), false);
    let b = BasicCred::new("".into(), "".into(), "b".into(), 80, "/".into(), false);
    assert!(a < b);
}

#[test]
fn test_factory_resolve_http_with_url_creds() {
    let mut factory = AuthConfigFactory::new();
    let url = Url::parse("http://user:pass@example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, true, &opts).unwrap();
    assert_eq!(ac.user(), "user");
    assert_eq!(ac.password(), "pass");
}

#[test]
fn test_factory_resolve_http_no_creds() {
    let mut factory = AuthConfigFactory::new();
    let url = Url::parse("http://example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    assert!(factory.resolve(&url, false, &opts).is_none());
}

#[test]
fn test_factory_resolve_http_cli_fallback() {
    let mut factory = AuthConfigFactory::new();
    let url = Url::parse("http://example.com/file").unwrap();
    let opts = AuthResolveOptions {
        http_user: Some("cliuser".into()),
        http_passwd: Some("clipass".into()),
        ..Default::default()
    };
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "cliuser");
    assert_eq!(ac.password(), "clipass");
}

#[test]
fn test_factory_resolve_http_challenge_mode() {
    let mut factory = AuthConfigFactory::new();
    let url = Url::parse("http://example.com/protected").unwrap();
    let opts = AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("challenge_user".into()),
        http_passwd: Some("challenge_pass".into()),
        ..Default::default()
    };

    // No cached cred yet — should return None
    assert!(factory.resolve(&url, false, &opts).is_none());

    // Activate BasicCred for this host
    let activated = factory.activate_basic_cred("example.com", 80, "/protected", &opts);
    assert!(activated);

    // Now resolve should return the activated cred
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "challenge_user");
}

#[test]
fn test_factory_resolve_ftp_anonymous_default() {
    let mut factory = AuthConfigFactory::new();
    let url = Url::parse("ftp://ftp.example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "anonymous");
    assert_eq!(ac.password(), "ARIA2USER@");
}

#[test]
fn test_factory_resolve_ftp_with_netrc() {
    let mut factory = AuthConfigFactory::new();
    let netrc = NetrcStore::from_entries(vec![NetrcEntry {
        host: "ftp.example.com".into(),
        login: "netrc_user".into(),
        password: "netrc_pass".into(),
    }]);
    factory.set_netrc(netrc);

    let url = Url::parse("ftp://ftp.example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "netrc_user");
    assert_eq!(ac.password(), "netrc_pass");
}

#[test]
fn test_factory_update_basic_cred_replaces() {
    let mut factory = AuthConfigFactory::new();
    factory.update_basic_cred(BasicCred::new(
        "old".into(),
        "oldpass".into(),
        "h".into(),
        80,
        "/".into(),
        false,
    ));
    assert_eq!(factory.basic_cred_count(), 1);

    // Same host/port/path — should replace, not duplicate
    factory.update_basic_cred(BasicCred::new(
        "new".into(),
        "newpass".into(),
        "h".into(),
        80,
        "/".into(),
        true,
    ));
    assert_eq!(factory.basic_cred_count(), 1);

    let found = factory.find_basic_cred("h", 80, "/");
    assert!(found.is_some());
    assert_eq!(found.unwrap().user, "new");
}

#[test]
fn test_erase_confidential_info() {
    let raw = "GET / HTTP/1.1\r\nAuthorization: Basic dXNlcjpwYXNz\r\nCookie: session=abc\r\nX-Custom: visible\r\n";
    let safe = erase_confidential_info(raw);
    assert!(safe.contains("Authorization: <snip>"));
    assert!(safe.contains("Cookie: <snip>"));
    assert!(safe.contains("X-Custom: visible"));
    assert!(!safe.contains("Basic dXNlcjpwYXNz"));
    assert!(!safe.contains("session=abc"));
}

#[test]
fn test_erase_confidential_proxy_auth() {
    let raw = "CONNECT host:443 HTTP/1.1\r\nProxy-Authorization: Digest user=test\r\nSet-Cookie: id=123\r\n";
    let safe = erase_confidential_info(raw);
    assert!(safe.contains("Proxy-Authorization: <snip>"));
    assert!(safe.contains("Set-Cookie: <snip>"));
}

// ---------------------------------------------------------------------------
// Netrc integration tests (parser -> NetrcStore -> AuthConfigFactory)
// ---------------------------------------------------------------------------

#[test]
fn test_netrc_to_auth_factory_http() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("machine http.example.com\nlogin netrcuser\npassword netrcpass\n")
        .unwrap();

    let url = Url::parse("http://http.example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "netrcuser");
    assert_eq!(ac.password(), "netrcpass");
}

#[test]
fn test_netrc_to_auth_factory_ftp() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("machine ftp.example.com\nlogin ftpuser\npassword ftppass\n")
        .unwrap();

    let url = Url::parse("ftp://ftp.example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "ftpuser");
    assert_eq!(ac.password(), "ftppass");
}

#[test]
fn test_netrc_domain_suffix_matches_http_subdomain() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("machine .example.com\nlogin netrcuser\npassword netrcpass\n")
        .unwrap();

    let url = Url::parse("http://cdn.example.com/file").unwrap();
    let ac = factory
        .resolve(&url, false, &AuthResolveOptions::default())
        .unwrap();
    assert_eq!(ac.user(), "netrcuser");

    let bare_domain = Url::parse("http://example.com/file").unwrap();
    assert!(
        factory
            .resolve(&bare_domain, false, &AuthResolveOptions::default())
            .is_none()
    );
}

#[test]
fn test_netrc_default_fallback_for_http() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str(
            "machine known.example.com\nlogin knownuser\npassword knownpass\n\
         default\nlogin defaultuser\npassword defaultpass\n",
        )
        .unwrap();

    // Known host gets its specific entry
    let url = Url::parse("http://known.example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "knownuser");

    // HTTP resolver ignores the .netrc default entry, as C++ does.
    let url2 = Url::parse("http://unknown.example.com/file").unwrap();
    assert!(factory.resolve(&url2, false, &opts).is_none());
}

#[test]
fn test_netrc_default_fallback_for_ftp() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("default\nlogin anonftp\npassword anonftp@\n")
        .unwrap();

    let url = Url::parse("ftp://any.host.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    assert_eq!(ac.user(), "anonftp");
}

#[test]
fn test_netrc_no_netrc_flag_skips_lookup() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("machine example.com\nlogin netrcuser\npassword netrcpass\n")
        .unwrap();

    let url = Url::parse("http://example.com/file").unwrap();
    let opts = AuthResolveOptions {
        no_netrc: true,
        http_user: Some("cliuser".into()),
        http_passwd: Some("clipass".into()),
        ..Default::default()
    };
    let ac = factory.resolve(&url, false, &opts).unwrap();
    // Should use CLI creds, not netrc
    assert_eq!(ac.user(), "cliuser");
}

#[test]
fn test_netrc_url_creds_take_priority() {
    let mut factory = AuthConfigFactory::new();
    factory
        .load_netrc_str("machine example.com\nlogin netrcuser\npassword netrcpass\n")
        .unwrap();

    let url = Url::parse("http://urluser:urlpass@example.com/file").unwrap();
    let opts = AuthResolveOptions::default();
    let ac = factory.resolve(&url, false, &opts).unwrap();
    // URL creds take priority over netrc
    assert_eq!(ac.user(), "urluser");
    assert_eq!(ac.password(), "urlpass");
}

#[test]
fn test_netrc_parse_error_propagates() {
    let mut factory = AuthConfigFactory::new();
    let result = factory.load_netrc_str("login user\n");
    assert!(result.is_err());
    match result.unwrap_err() {
        NetrcError::ParseError(msg) => {
            assert!(msg.contains("machine") || msg.contains("default"));
        }
        other => panic!("expected ParseError, got {:?}", other),
    }
}
