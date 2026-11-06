use super::pkcs12::{
    decrypt_pkcs12_aes_gcm, is_pkcs12_private_key_bag, passwordless_pkcs12_bags,
    pkcs5_algorithm_der, pkcs12_oid,
};
use super::test_support::{
    TEST_CHAIN_PEM, TEST_PRIVATE_KEY_PEM, TEST_ROOT_PEM, pem_certificates, pem_private_key,
    start_https_fixture, test_client_builder,
};
use super::{ClientTlsConfig, apply};

fn passwordless_pkcs12_identity(password: &p12_q3::BmpString) -> Vec<u8> {
    let certificates = pem_certificates(TEST_CHAIN_PEM);
    let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);
    let ca_certificates = certificates
        .iter()
        .skip(1)
        .map(|certificate| certificate.as_ref())
        .collect::<Vec<_>>();

    p12_q3::PFX::new_with_cas(
        certificates[0].as_ref(),
        private_key.secret_der(),
        &ca_certificates,
        password,
        "aria2-rust-test",
    )
    .expect("passwordless PKCS#12 fixture should be generated")
    .to_der()
}

fn passwordless_pkcs12_with_key_bag(key_bag: p12_q3::SafeBagKind) -> Vec<u8> {
    passwordless_pkcs12_with_key_bag_and_extras(key_bag, &[])
}

fn passwordless_pkcs12_with_key_bag_and_extras(
    key_bag: p12_q3::SafeBagKind,
    extra_bags: &[p12_q3::SafeBagKind],
) -> Vec<u8> {
    let certificates = pem_certificates(TEST_CHAIN_PEM);
    let password = p12_q3::BmpString::with_two_trailing_zeros("");
    let key_bag = p12_q3::SafeBag {
        bag: key_bag,
        attributes: vec![],
    };
    let cert_bag = p12_q3::SafeBag {
        bag: p12_q3::SafeBagKind::CertBag(p12_q3::CertBag::X509(certificates[0].as_ref().to_vec())),
        attributes: vec![],
    };
    let encrypted_certificates =
        p12_q3::EncryptedData::from_safe_bags(&[cert_bag], password.as_ref())
            .expect("certificate safe contents should encrypt");
    let authenticated_safe = yasna::construct_der(|writer| {
        writer.write_sequence_of(|writer| {
            p12_q3::ContentInfo::EncryptedData(encrypted_certificates).write(writer.next());
            p12_q3::ContentInfo::Data(yasna::construct_der(|writer| {
                writer.write_sequence_of(|writer| {
                    write_test_key_bag(writer.next(), &key_bag.bag);
                    for extra_bag in extra_bags {
                        write_test_key_bag(writer.next(), extra_bag);
                    }
                });
            }))
            .write(writer.next());
        });
    });

    p12_q3::PFX {
        version: 3,
        auth_safe: p12_q3::ContentInfo::Data(authenticated_safe),
        mac_data: None,
    }
    .to_der()
}

fn write_test_key_bag(writer: yasna::DERWriter, bag: &p12_q3::SafeBagKind) {
    writer.write_sequence(|writer| {
        writer.next().write_oid(&bag.oid());
        writer
            .next()
            .write_tagged(yasna::Tag::context(0), |writer| match bag {
                p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) => {
                    let algorithm_der = pkcs5_algorithm_der(&key_bag.encryption_algorithm)
                        .unwrap_or_else(|| {
                            yasna::construct_der(|writer| {
                                key_bag.encryption_algorithm.write(writer)
                            })
                        });
                    writer.write_sequence(|writer| {
                        writer.next().write_der(&algorithm_der);
                        writer.next().write_bytes(&key_bag.encrypted_data);
                    });
                }
                _ => bag.write(writer),
            });
    });
}

fn aes_cbc_pkcs12_key_bag(algorithm_oid: &[u64]) -> p12_q3::SafeBagKind {
    aes_cbc_pkcs12_key_bag_with_prf(algorithm_oid, p12_q3::AlgorithmIdentifier::HmacWithSha256)
}

fn aes_cbc_pkcs12_key_bag_with_prf(
    algorithm_oid: &[u64],
    prf: p12_q3::AlgorithmIdentifier,
) -> p12_q3::SafeBagKind {
    let password = p12_q3::BmpString::with_two_trailing_zeros("");
    let iv = [7u8; 16];
    let algorithm = p12_q3::AlgorithmIdentifier::Pbes2(p12_q3::Pkcs12Pbes2Params {
        key_derivation_function: Box::new(p12_q3::AlgorithmIdentifier::Pbkdf2(
            p12_q3::Pbkdf2Params {
                salt: p12_q3::Pbkdf2Salt::Specified(b"rust-owned-salt".to_vec()),
                iteration_count: 1000,
                key_length: None,
                prf: Box::new(prf),
            },
        )),
        encryption_scheme: Box::new(p12_q3::AlgorithmIdentifier::OtherAlg(
            p12_q3::OtherAlgorithmIdentifier {
                algorithm_type: pkcs12_oid(algorithm_oid),
                params: Some(yasna::construct_der(|writer| writer.write_bytes(&iv))),
            },
        )),
    });
    let algorithm_der =
        pkcs5_algorithm_der(&algorithm).expect("test AES-CBC PBES2 algorithm should be supported");
    let scheme = pkcs5::EncryptionScheme::try_from(algorithm_der.as_slice())
        .expect("test AES-CBC PBES2 algorithm should parse");
    let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);
    let encrypted_data = scheme
        .encrypt(password.as_ref(), private_key.secret_der())
        .expect("test private key should encrypt");

    p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(p12_q3::EncryptedPrivateKeyInfo {
        encryption_algorithm: algorithm,
        encrypted_data,
    })
}

fn aes_gcm_pkcs12_key_bag(algorithm_oid: &[u64], tag_len: usize) -> p12_q3::SafeBagKind {
    use aes_gcm::aead::consts::U12;
    use aes_gcm::aead::{AeadInPlace, KeyInit};

    let password = p12_q3::BmpString::with_two_trailing_zeros("");
    let salt = b"rust-owned-gcm-salt";
    let nonce = [9u8; 12];
    let key_len = if algorithm_oid.last() == Some(&6) {
        16
    } else {
        32
    };
    let mut key = vec![0u8; key_len];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_ref(), salt, 1000, &mut key);
    let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);
    let mut encrypted_data = private_key.secret_der().to_vec();
    let tag: Vec<u8> = if key_len == 16 && tag_len == 12 {
        let cipher = aes_gcm::AesGcm::<aes_gcm::aes::Aes128, U12, U12>::new_from_slice(&key)
            .expect("AES-128-GCM test key should have the right length");
        cipher
            .encrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut encrypted_data)
            .expect("AES-128-GCM test encryption should succeed")
            .to_vec()
    } else if key_len == 16 {
        let cipher = aes_gcm::Aes128Gcm::new_from_slice(&key)
            .expect("AES-128-GCM test key should have the right length");
        cipher
            .encrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut encrypted_data)
            .expect("AES-128-GCM test encryption should succeed")
            .to_vec()
    } else if tag_len == 12 {
        let cipher = aes_gcm::AesGcm::<aes_gcm::aes::Aes256, U12, U12>::new_from_slice(&key)
            .expect("AES-256-GCM test key should have the right length");
        cipher
            .encrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut encrypted_data)
            .expect("AES-256-GCM test encryption should succeed")
            .to_vec()
    } else {
        let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key)
            .expect("AES-256-GCM test key should have the right length");
        cipher
            .encrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut encrypted_data)
            .expect("AES-256-GCM test encryption should succeed")
            .to_vec()
    };
    encrypted_data.extend_from_slice(&tag);

    let algorithm = p12_q3::AlgorithmIdentifier::Pbes2(p12_q3::Pkcs12Pbes2Params {
        key_derivation_function: Box::new(p12_q3::AlgorithmIdentifier::Pbkdf2(
            p12_q3::Pbkdf2Params {
                salt: p12_q3::Pbkdf2Salt::Specified(salt.to_vec()),
                iteration_count: 1000,
                key_length: None,
                prf: Box::new(p12_q3::AlgorithmIdentifier::HmacWithSha256),
            },
        )),
        encryption_scheme: Box::new(p12_q3::AlgorithmIdentifier::OtherAlg(
            p12_q3::OtherAlgorithmIdentifier {
                algorithm_type: pkcs12_oid(algorithm_oid),
                params: Some(yasna::construct_der(|writer| {
                    writer.write_sequence(|writer| {
                        writer.next().write_bytes(&nonce);
                        if tag_len != 12 {
                            writer.next().write_u64(tag_len as u64);
                        }
                    });
                })),
            },
        )),
    });

    p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(p12_q3::EncryptedPrivateKeyInfo {
        encryption_algorithm: algorithm,
        encrypted_data,
    })
}

#[test]
fn accepts_alternative_pbkdf2_prfs_for_pkcs12_private_keys() {
    crate::http::client_pool::ensure_rustls_provider();
    for (prf_oid, prf_name) in [
        ([1, 2, 840, 113549, 2, 8], "SHA-224"),
        ([1, 2, 840, 113549, 2, 10], "SHA-384"),
        ([1, 2, 840, 113549, 2, 11], "SHA-512"),
    ] {
        let prf = p12_q3::AlgorithmIdentifier::OtherAlg(p12_q3::OtherAlgorithmIdentifier {
            algorithm_type: pkcs12_oid(&prf_oid),
            params: Some(yasna::construct_der(|writer| writer.write_null())),
        });
        let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
        let certificate_path = directory.path().join("client.p12");
        let archive = passwordless_pkcs12_with_key_bag(aes_cbc_pkcs12_key_bag_with_prf(
            &[2, 16, 840, 1, 101, 3, 4, 1, 2],
            prf,
        ));
        std::fs::write(&certificate_path, archive).expect("write alternative-PRF PKCS#12 fixture");

        let builder = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some(certificate_path.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .unwrap_or_else(|error| {
            panic!("PKCS#12 identity with PBKDF2 {prf_name} should configure: {error}")
        });
        builder
            .build()
            .unwrap_or_else(|error| panic!("PBKDF2 {prf_name} identity should build: {error}"));
    }
}

#[test]
fn accepts_aes128_and_aes192_cbc_pkcs12_private_keys() {
    crate::http::client_pool::ensure_rustls_provider();
    for algorithm_oid in [
        [2, 16, 840, 1, 101, 3, 4, 1, 2],
        [2, 16, 840, 1, 101, 3, 4, 1, 22],
    ] {
        let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
        let certificate_path = directory.path().join("client.p12");
        let archive = passwordless_pkcs12_with_key_bag(aes_cbc_pkcs12_key_bag(&algorithm_oid));
        std::fs::write(&certificate_path, archive).expect("write AES-CBC PKCS#12 fixture");
        let archive = std::fs::read(&certificate_path).expect("read AES-CBC PKCS#12 fixture");
        let pfx = p12_q3::PFX::parse(&archive).expect("AES-CBC PKCS#12 fixture should parse");
        let password = p12_q3::BmpString::with_two_trailing_zeros("");
        let bags = pfx
            .bags(&password)
            .expect("AES-CBC PKCS#12 bags should parse");
        let key_bag = bags
            .iter()
            .find_map(|bag| match &bag.bag {
                p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) => Some(key_bag),
                _ => None,
            })
            .expect("AES-CBC PKCS#12 fixture should contain a key");
        let algorithm_der = pkcs5_algorithm_der(&key_bag.encryption_algorithm)
            .expect("AES-CBC PBES2 algorithm should convert to PKCS#5 DER");
        let scheme = pkcs5::EncryptionScheme::try_from(algorithm_der.as_slice())
            .expect("AES-CBC PBES2 algorithm should parse as PKCS#5");
        assert!(
            scheme
                .decrypt(password.as_ref(), &key_bag.encrypted_data)
                .is_ok()
        );

        let builder = apply(
            reqwest::Client::builder(),
            &ClientTlsConfig {
                certificate: Some(certificate_path.to_string_lossy().into_owned()),
                ..ClientTlsConfig::default()
            },
        )
        .expect("AES-128/192-CBC PKCS#12 identity should configure the client");
        builder
            .build()
            .expect("AES-128/192-CBC PKCS#12 client identity should build");
    }
}

#[test]
fn accepts_aes128_and_aes256_gcm_pkcs12_private_keys() {
    crate::http::client_pool::ensure_rustls_provider();
    for algorithm_oid in [
        [2, 16, 840, 1, 101, 3, 4, 1, 6],
        [2, 16, 840, 1, 101, 3, 4, 1, 46],
    ] {
        for tag_len in [12, 16] {
            let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
            let certificate_path = directory.path().join("client-gcm.p12");
            let archive =
                passwordless_pkcs12_with_key_bag(aes_gcm_pkcs12_key_bag(&algorithm_oid, tag_len));
            std::fs::write(&certificate_path, archive).expect("write AES-GCM PKCS#12 fixture");
            let archive = std::fs::read(&certificate_path).expect("read AES-GCM PKCS#12 fixture");
            let pfx = p12_q3::PFX::parse(&archive).expect("AES-GCM PKCS#12 fixture should parse");
            let password = p12_q3::BmpString::with_two_trailing_zeros("");
            let bags = passwordless_pkcs12_bags(&pfx, &password)
                .expect("AES-GCM PKCS#12 bags should parse");
            let key_bag = bags
                .iter()
                .find(|bag| is_pkcs12_private_key_bag(bag))
                .expect("AES-GCM PKCS#12 fixture should contain a key");
            let p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) = &key_bag.bag else {
                panic!("expected shrouded key bag");
            };
            assert!(
                decrypt_pkcs12_aes_gcm(
                    &key_bag.encryption_algorithm,
                    &key_bag.encrypted_data,
                    &password,
                )
                .is_some(),
                "AES-GCM private key should decrypt"
            );

            apply(
                reqwest::Client::builder(),
                &ClientTlsConfig {
                    certificate: Some(certificate_path.to_string_lossy().into_owned()),
                    ..ClientTlsConfig::default()
                },
            )
            .expect("AES-GCM PKCS#12 identity should configure the client")
            .build()
            .expect("AES-GCM PKCS#12 client identity should build");
        }
    }
}

#[test]
fn accepts_a_plaintext_pkcs12_key_bag() {
    crate::http::client_pool::ensure_rustls_provider();
    let private_key = pem_private_key(TEST_PRIVATE_KEY_PEM);
    let key_bag = p12_q3::SafeBagKind::OtherBagKind(p12_q3::OtherBag {
        bag_id: pkcs12_oid(&[1, 2, 840, 113549, 1, 12, 10, 1, 1]),
        bag_value: private_key.secret_der().to_vec(),
    });
    let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
    let certificate_path = directory.path().join("client.p12");
    std::fs::write(&certificate_path, passwordless_pkcs12_with_key_bag(key_bag))
        .expect("write plaintext keyBag PKCS#12 fixture");

    let builder = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some(certificate_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect("plaintext keyBag PKCS#12 identity should configure the client");
    builder
        .build()
        .expect("plaintext keyBag PKCS#12 client identity should build");
}

#[test]
fn ignores_unknown_pkcs12_bags_when_certificate_and_key_are_present() {
    crate::http::client_pool::ensure_rustls_provider();
    let unknown_bag = p12_q3::SafeBagKind::OtherBagKind(p12_q3::OtherBag {
        bag_id: pkcs12_oid(&[1, 2, 840, 113549, 1, 12, 10, 1, 5]),
        bag_value: yasna::construct_der(|writer| writer.write_bytes(b"ignored")),
    });
    let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
    let certificate_path = directory.path().join("client-with-unknown-bag.p12");
    let archive = passwordless_pkcs12_with_key_bag_and_extras(
        aes_cbc_pkcs12_key_bag(&[2, 16, 840, 1, 101, 3, 4, 1, 2]),
        &[unknown_bag],
    );
    std::fs::write(&certificate_path, archive).expect("write unknown-bag PKCS#12 fixture");

    apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some(certificate_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect("unknown PKCS#12 bags should be ignored when identity is usable")
    .build()
    .expect("PKCS#12 identity with an unknown bag should build");
}

#[tokio::test]
async fn passwordless_pkcs12_identity_completes_live_mutual_tls() {
    let fixture = start_https_fixture(true).await;
    let directory = tempfile::tempdir().expect("create temporary identity directory");
    let ca_path = directory.path().join("root.pem");
    let certificate_path = directory.path().join("client.p12");
    let password = p12_q3::BmpString::with_two_trailing_zeros("");
    std::fs::write(&ca_path, TEST_ROOT_PEM).expect("write test root certificate");
    std::fs::write(&certificate_path, passwordless_pkcs12_identity(&password))
        .expect("write passwordless PKCS#12 fixture");

    let config = ClientTlsConfig {
        ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
        certificate: Some(certificate_path.to_string_lossy().into_owned()),
        ..ClientTlsConfig::default()
    };
    let client = apply(test_client_builder(fixture.address), &config)
        .expect("passwordless PKCS#12 identity should configure the client")
        .build()
        .expect("passwordless PKCS#12 client should build");

    let response = client
        .get(fixture.url())
        .send()
        .await
        .expect("passwordless PKCS#12 identity should complete mutual TLS");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.bytes().await.expect("read HTTPS response"),
        bytes::Bytes::from_static(b"tls-ok")
    );
}

#[test]
fn accepts_empty_password_without_bmp_terminator() {
    crate::http::client_pool::ensure_rustls_provider();
    let directory = tempfile::tempdir().expect("create temporary PKCS#12 directory");
    let certificate_path = directory.path().join("client.p12");
    let password = p12_q3::BmpString::empty_without_trailing_zeros();
    std::fs::write(&certificate_path, passwordless_pkcs12_identity(&password))
        .expect("write passwordless PKCS#12 fixture");

    let builder = apply(
        reqwest::Client::builder(),
        &ClientTlsConfig {
            certificate: Some(certificate_path.to_string_lossy().into_owned()),
            ..ClientTlsConfig::default()
        },
    )
    .expect("empty password without a BMP terminator should be accepted");
    builder
        .build()
        .expect("passwordless PKCS#12 client should build");
}
