use super::pkcs12::empty_pkcs12_password;
use super::test_support::{
    TEST_CHAIN_PEM, TEST_PRIVATE_KEY_PEM, TEST_ROOT_PEM, TLS12_ONLY, start_https_fixture,
    start_https_fixture_with_versions, test_client_builder,
};
use super::{ClientTlsConfig, apply};
use bytes::Bytes;

#[test]
fn accepts_an_unconfigured_identity() {
    crate::http::client_pool::ensure_rustls_provider();
    let builder = apply(reqwest::Client::builder(), &ClientTlsConfig::default())
        .expect("missing optional client identity should be valid");
    builder.build().expect("plain client should build");
}

#[test]
fn marks_non_default_tls_settings_as_custom_client_requirements() {
    assert!(!ClientTlsConfig::default().requires_custom_client());
    assert!(
        ClientTlsConfig {
            check_certificate: false,
            ..ClientTlsConfig::default()
        }
        .requires_custom_client()
    );
    assert!(
        ClientTlsConfig {
            ca_certificate: Some("ca.pem".into()),
            ..ClientTlsConfig::default()
        }
        .requires_custom_client()
    );
    assert!(
        ClientTlsConfig {
            min_tls_version: Some("TLSv1.3".into()),
            ..ClientTlsConfig::default()
        }
        .requires_custom_client()
    );
}

#[test]
fn rejects_unknown_minimum_tls_version() {
    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            min_tls_version: Some("TLSv9".into()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("unknown minimum TLS versions must be rejected");
    assert!(
        error
            .to_string()
            .contains("Unsupported minimum TLS version")
    );
}

#[test]
fn accepts_disabled_certificate_verification() {
    crate::http::client_pool::ensure_rustls_provider();
    let builder = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            check_certificate: false,
            ..ClientTlsConfig::default()
        },
    )
    .expect("disabled certificate verification should be configurable");
    builder
        .build()
        .expect("client with disabled certificate verification should build");
}

#[test]
fn reports_missing_ca_certificate_file() {
    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            ca_certificate: Some("missing-ca.pem".into()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("missing CA files must be rejected");
    assert!(error.to_string().contains("Failed to read CA certificate"));
}

#[test]
fn reports_invalid_ca_certificate_pem() {
    let directory = tempfile::tempdir().expect("create temporary CA directory");
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, b"not a CA certificate").expect("write invalid CA fixture");

    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            ca_certificate: Some(ca.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("invalid CA PEM must be rejected");
    assert!(error.to_string().contains("Invalid CA certificate"));
}

#[test]
fn rejects_a_non_pkcs12_certificate_when_private_key_is_omitted() {
    let directory = tempfile::tempdir().expect("create temporary identity directory");
    let certificate = directory.path().join("client.pem");
    std::fs::write(&certificate, b"not a PKCS#12 archive").expect("write invalid PKCS#12 fixture");

    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some(certificate.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("non-PKCS#12 certificates without a key must be rejected");
    assert!(
        error
            .to_string()
            .contains("Invalid empty-password PKCS#12 client identity")
    );
}

#[test]
fn accepts_a_modern_empty_password_pkcs12_identity() {
    crate::http::client_pool::ensure_rustls_provider();
    let archive = include_bytes!("../testdata/modern_empty_password_aes256.p12");
    let pfx = p12_q3::PFX::parse(archive).expect("modern PKCS#12 fixture should parse");
    assert!(matches!(
        pfx.mac_data
            .as_ref()
            .map(|mac_data| &mac_data.mac.digest_algorithm),
        Some(p12_q3::AlgorithmIdentifier::Sha2)
    ));
    let password = empty_pkcs12_password(&pfx)
        .expect("modern PKCS#12 fixture should verify with an empty password");
    let bags = pfx
        .bags(&password)
        .expect("modern PKCS#12 fixture bags should decrypt");
    let key_bag = bags
        .iter()
        .find_map(|bag| match &bag.bag {
            p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) => Some(key_bag),
            _ => None,
        })
        .expect("modern PKCS#12 fixture should contain a private key");
    let p12_q3::AlgorithmIdentifier::Pbes2(params) = &key_bag.encryption_algorithm else {
        panic!("modern PKCS#12 fixture should use PBES2");
    };
    assert!(matches!(
        params.key_derivation_function.as_ref(),
        p12_q3::AlgorithmIdentifier::Pbkdf2(params)
            if matches!(
                params.prf.as_ref(),
                p12_q3::AlgorithmIdentifier::HmacWithSha256
            )
    ));
    assert!(matches!(
        params.encryption_scheme.as_ref(),
        p12_q3::AlgorithmIdentifier::AesCbcPad(iv) if iv.len() == 16
    ));

    let directory = tempfile::tempdir().expect("create temporary identity directory");
    let certificate = directory.path().join("modern-client.p12");
    std::fs::write(&certificate, archive).expect("write modern PKCS#12 fixture");

    let builder = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some(certificate.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect("modern empty-password PKCS#12 identity should configure the client");
    builder
        .build()
        .expect("modern PKCS#12 client identity should build");
}

#[test]
fn rejects_an_unpaired_private_key() {
    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            private_key: Some("client.key".into()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("private key without a certificate must be rejected");
    assert!(error.to_string().contains("requires certificate"));
}

#[test]
fn reports_missing_certificate_file() {
    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some("missing-client.pem".into()),
            private_key: Some("missing-client.key".into()),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("missing certificate files must be rejected");
    assert!(
        error
            .to_string()
            .contains("Failed to read client certificate")
    );
}

#[test]
fn reports_invalid_certificate_and_key_pem() {
    let directory = tempfile::tempdir().expect("create temporary identity directory");
    let certificate = directory.path().join("client.pem");
    let private_key = directory.path().join("client.key");
    std::fs::write(&certificate, b"not a certificate").expect("write invalid certificate fixture");
    std::fs::write(&private_key, b"not a private key").expect("write invalid key fixture");

    let error = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: certificate.to_string_lossy().into_owned().into(),
            private_key: private_key.to_string_lossy().into_owned().into(),
            ..ClientTlsConfig::default()
        },
    )
    .expect_err("invalid PEM must be rejected");
    assert!(
        error
            .to_string()
            .contains("Invalid client certificate/private key")
    );
}

#[tokio::test]
async fn custom_ca_verifies_a_live_https_server() {
    let fixture = start_https_fixture(false).await;
    let directory = tempfile::tempdir().expect("create temporary CA directory");
    let ca_path = directory.path().join("root.pem");
    std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");

    let config = ClientTlsConfig {
        ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
        ..ClientTlsConfig::default()
    };
    let client = apply(test_client_builder(fixture.address), &config)
        .expect("custom CA should configure the client")
        .build()
        .expect("custom CA client should build");

    let response = client
        .get(fixture.url())
        .send()
        .await
        .expect("custom CA should verify the live server");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.bytes().await.expect("read HTTPS response"),
        Bytes::from_static(b"tls-ok")
    );
}

#[tokio::test]
async fn disabled_certificate_verification_reaches_a_live_https_server() {
    let fixture = start_https_fixture(false).await;
    let config = ClientTlsConfig {
        check_certificate: false,
        ..ClientTlsConfig::default()
    };
    let client = apply(test_client_builder(fixture.address), &config)
        .expect("disabled verification should configure the client")
        .build()
        .expect("disabled verification client should build");

    let response = client
        .get(fixture.url())
        .send()
        .await
        .expect("disabled verification should reach the live server");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.bytes().await.expect("read HTTPS response"),
        Bytes::from_static(b"tls-ok")
    );
}

#[tokio::test]
async fn minimum_tls_version_rejects_an_older_live_server_protocol() {
    let fixture = start_https_fixture_with_versions(false, TLS12_ONLY).await;
    let config = ClientTlsConfig {
        min_tls_version: Some("TLSv1.3".into()),
        ..ClientTlsConfig::default()
    };
    let client = apply(test_client_builder(fixture.address), &config)
        .expect("minimum TLS version should configure the client")
        .build()
        .expect("minimum TLS version client should build");

    let error = client
        .get(fixture.url())
        .send()
        .await
        .expect_err("TLS 1.3 minimum must reject a TLS 1.2-only server");
    let _ = error;
}

#[tokio::test]
async fn client_certificate_and_private_key_complete_live_mutual_tls() {
    let fixture = start_https_fixture(true).await;
    let directory = tempfile::tempdir().expect("create temporary identity directory");
    let ca_path = directory.path().join("root.pem");
    let certificate_path = directory.path().join("client-chain.pem");
    let private_key_path = directory.path().join("client.key");
    std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");
    std::fs::write(&certificate_path, TEST_CHAIN_PEM).expect("write test client certificate chain");
    std::fs::write(&private_key_path, TEST_PRIVATE_KEY_PEM).expect("write test client private key");

    let config = ClientTlsConfig {
        ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
        certificate: Some(certificate_path.to_string_lossy().into_owned()),
        private_key: Some(private_key_path.to_string_lossy().into_owned()),
        ..ClientTlsConfig::default()
    };
    let client = apply(test_client_builder(fixture.address), &config)
        .expect("client identity should configure the client")
        .build()
        .expect("mutual TLS client should build");

    let response = client
        .get(fixture.url())
        .send()
        .await
        .expect("client identity should complete mutual TLS");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.bytes().await.expect("read HTTPS response"),
        Bytes::from_static(b"tls-ok")
    );
}
