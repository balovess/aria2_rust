use super::*;

#[test]
fn ftps_config_default() {
    let config = FtpsConfig::default();
    assert!(!config.enabled);
    assert!(config.check_certificate);
    assert!(config.ca_certificate.is_none());
    assert_eq!(config.min_tls_version, TlsVersion::Tls12);
}

#[test]
fn tls_version_default() {
    assert_eq!(TlsVersion::default(), TlsVersion::Tls12);
}

#[test]
fn response_code_requires_ascii_three_digit_prefix() {
    assert_eq!(parse_response_code("200 OK", "TEST").unwrap(), 200);
    assert!(parse_response_code("20", "TEST").is_err());
    assert!(parse_response_code("é00 OK", "TEST").is_err());
    assert!(parse_response_code("ABC OK", "TEST").is_err());
}

#[test]
fn connector_builds_with_check_enabled() {
    let config = FtpsConfig {
        enabled: true,
        check_certificate: true,
        ca_certificate: None,
        min_tls_version: TlsVersion::Tls12,
    };
    assert!(build_tls_connector(&config).is_ok());
}

#[test]
fn connector_builds_with_check_disabled() {
    let config = FtpsConfig {
        enabled: true,
        check_certificate: false,
        ca_certificate: None,
        min_tls_version: TlsVersion::Tls12,
    };
    assert!(build_tls_connector(&config).is_ok());
}

#[test]
fn connector_rejects_missing_ca_file() {
    let config = FtpsConfig {
        enabled: true,
        check_certificate: true,
        ca_certificate: Some(PathBuf::from("/nonexistent/ca.pem")),
        min_tls_version: TlsVersion::Tls12,
    };
    assert!(build_tls_connector(&config).is_err());
}

#[test]
fn connector_builds_with_tls13() {
    let config = FtpsConfig {
        enabled: true,
        check_certificate: true,
        ca_certificate: None,
        min_tls_version: TlsVersion::Tls13,
    };
    assert!(build_tls_connector(&config).is_ok());
}

#[tokio::test]
async fn protected_data_channel_negotiation_requires_both_responses() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (client, server) = tokio::io::duplex(256);
    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(server);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "PBSZ 0\r\n");
        reader
            .get_mut()
            .write_all(b"200 PBSZ ok\r\n")
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "PROT P\r\n");
        reader
            .get_mut()
            .write_all(b"200 PROT ok\r\n")
            .await
            .unwrap();
    });

    let mut client = tokio::io::BufReader::new(client);
    negotiate_protected_data_channel(&mut client).await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn protected_data_channel_negotiation_fails_closed_on_prot_rejection() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (client, server) = tokio::io::duplex(256);
    let server = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(server);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        reader
            .get_mut()
            .write_all(b"200 PBSZ ok\r\n")
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        reader
            .get_mut()
            .write_all(b"534 PROT rejected\r\n")
            .await
            .unwrap();
    });

    let mut client = tokio::io::BufReader::new(client);
    let result = negotiate_protected_data_channel(&mut client).await;
    assert!(result.is_err());
    server.await.unwrap();
}
