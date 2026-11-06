use crate::error::{Aria2Error, Result};

pub(super) fn load_empty_password_identity(certificate: &str) -> Result<reqwest::Identity> {
    let archive = std::fs::read(certificate).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Failed to read client certificate '{}': {}",
            certificate, error
        )))
    })?;
    // p12_q3 uses debug assertions for a few malformed or unknown ASN.1
    // variants. PFX is user-provided input, so keep those assertions inside
    // the configuration boundary and report them as ordinary config errors.
    let pfx = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        p12_q3::PFX::parse(&archive)
    }))
    .map_err(|_| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity '{}': unsupported ASN.1 variant",
            certificate
        )))
    })?
    .map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity '{}': {error:?}",
            certificate
        )))
    })?;
    let password = empty_pkcs12_password(&pfx).ok_or_else(|| {
        Aria2Error::Fatal(crate::error::FatalError::Config(
            "Invalid empty-password PKCS#12 client identity: MAC verification failed".into(),
        ))
    })?;

    let bags = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pfx.bags(&password)))
    {
        Ok(Ok(bags)) => bags,
        Ok(Err(_)) | Err(_) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            passwordless_pkcs12_bags(&pfx, &password)
        }))
        .ok()
        .flatten()
        .ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Unsupported empty-password PKCS#12 encryption algorithm".into(),
            ))
        })?,
    };
    let key_bag = bags
        .iter()
        .find(|bag| is_pkcs12_private_key_bag(bag))
        .ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Empty-password PKCS#12 client identity does not contain a private key".into(),
            ))
        })?;
    let key = decrypt_pkcs12_private_key(key_bag, &password).ok_or_else(|| {
        Aria2Error::Fatal(crate::error::FatalError::Config(
            "Unable to decrypt empty-password PKCS#12 client private key".into(),
        ))
    })?;
    let key_id = key_bag.local_key_id();

    let mut leaf = None;
    let mut chain = Vec::new();
    for bag in bags {
        let Some(cert) = bag.bag.get_x509_cert() else {
            continue;
        };
        let matches_key = key_id
            .as_deref()
            .is_some_and(|key_id| bag.local_key_id().as_deref() == Some(key_id));
        if leaf.is_none() && matches_key {
            leaf = Some(cert);
        } else {
            chain.push(cert);
        }
    }

    let (leaf, chain) = if let Some(leaf) = leaf {
        (leaf, chain)
    } else {
        let mut certificates = chain.into_iter();
        let leaf = certificates.next().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Empty-password PKCS#12 client identity does not contain a certificate".into(),
            ))
        })?;
        (leaf, certificates.collect())
    };

    let mut identity = pem_block("CERTIFICATE", &leaf);
    for certificate in chain {
        identity.extend_from_slice(&pem_block("CERTIFICATE", &certificate));
    }
    identity.extend_from_slice(&pem_block("PRIVATE KEY", &key));
    reqwest::Identity::from_pem(&identity).map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Invalid empty-password PKCS#12 client identity: {}",
            error
        )))
    })
}

pub(super) fn pkcs12_oid(arcs: &[u64]) -> yasna::models::ObjectIdentifier {
    yasna::models::ObjectIdentifier::from_slice(arcs)
}

fn is_pkcs5_hmac_prf(algorithm: &yasna::models::ObjectIdentifier) -> bool {
    [
        &[1, 2, 840, 113549, 2, 8][..],
        &[1, 2, 840, 113549, 2, 10][..],
        &[1, 2, 840, 113549, 2, 11][..],
    ]
    .iter()
    .any(|arcs| algorithm == &pkcs12_oid(arcs))
}

fn has_der_null_parameters(params: Option<&[u8]>) -> bool {
    params == Some(&[0x05, 0x00])
}

fn is_supported_pkcs5_algorithm(algorithm: &p12_q3::AlgorithmIdentifier) -> bool {
    match algorithm {
        p12_q3::AlgorithmIdentifier::Pbes2(params) => {
            is_supported_pkcs5_algorithm(&params.key_derivation_function)
                && is_supported_pkcs5_algorithm(&params.encryption_scheme)
        }
        p12_q3::AlgorithmIdentifier::Pbkdf2(params) => {
            matches!(&params.salt, p12_q3::Pbkdf2Salt::Specified(_))
                && is_supported_pkcs5_algorithm(&params.prf)
        }
        p12_q3::AlgorithmIdentifier::HmacWithSha1
        | p12_q3::AlgorithmIdentifier::HmacWithSha256
        | p12_q3::AlgorithmIdentifier::AesCbcPad(_) => true,
        p12_q3::AlgorithmIdentifier::OtherAlg(other) => {
            other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 2])
                || other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 6])
                || other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 22])
                || other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 46])
                || (is_pkcs5_hmac_prf(&other.algorithm_type)
                    && has_der_null_parameters(other.params.as_deref()))
        }
        _ => false,
    }
}

fn write_pkcs5_algorithm(writer: yasna::DERWriter, algorithm: &p12_q3::AlgorithmIdentifier) {
    writer.write_sequence(|writer| match algorithm {
        p12_q3::AlgorithmIdentifier::HmacWithSha1 => {
            writer
                .next()
                .write_oid(&pkcs12_oid(&[1, 2, 840, 113549, 2, 7]));
            writer.next().write_null();
        }
        p12_q3::AlgorithmIdentifier::HmacWithSha256 => {
            writer
                .next()
                .write_oid(&pkcs12_oid(&[1, 2, 840, 113549, 2, 9]));
            writer.next().write_null();
        }
        p12_q3::AlgorithmIdentifier::Pbkdf2(params) => {
            writer
                .next()
                .write_oid(&pkcs12_oid(&[1, 2, 840, 113549, 1, 5, 12]));
            writer.next().write_sequence(|writer| {
                match &params.salt {
                    p12_q3::Pbkdf2Salt::Specified(salt) => writer.next().write_bytes(salt),
                    p12_q3::Pbkdf2Salt::OtherSource(_) => unreachable!(
                        "unsupported PBKDF2 salt source passed after algorithm validation"
                    ),
                }
                writer.next().write_u64(params.iteration_count);
                if let Some(key_length) = params.key_length {
                    writer.next().write_u64(key_length);
                }
                if !matches!(
                    params.prf.as_ref(),
                    p12_q3::AlgorithmIdentifier::HmacWithSha1
                ) {
                    write_pkcs5_algorithm(writer.next(), &params.prf);
                }
            });
        }
        p12_q3::AlgorithmIdentifier::AesCbcPad(iv) => {
            writer
                .next()
                .write_oid(&pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 42]));
            writer.next().write_bytes(iv);
        }
        p12_q3::AlgorithmIdentifier::OtherAlg(other) => {
            writer.next().write_oid(&other.algorithm_type);
            if let Some(params) = &other.params {
                writer.next().write_der(params);
            }
        }
        _ => unreachable!("unsupported PBES2 algorithm passed after validation"),
    });
}

pub(super) fn pkcs5_algorithm_der(algorithm: &p12_q3::AlgorithmIdentifier) -> Option<Vec<u8>> {
    if !is_supported_pkcs5_algorithm(algorithm) {
        return None;
    }

    Some(yasna::construct_der(|writer| {
        writer.write_sequence(|writer| {
            writer
                .next()
                .write_oid(&pkcs12_oid(&[1, 2, 840, 113549, 1, 5, 13]));
            let p12_q3::AlgorithmIdentifier::Pbes2(params) = algorithm else {
                unreachable!("PBES2 parameters were validated before encoding")
            };
            writer.next().write_sequence(|writer| {
                write_pkcs5_algorithm(writer.next(), &params.key_derivation_function);
                write_pkcs5_algorithm(writer.next(), &params.encryption_scheme);
            });
        });
    }))
}

fn decrypt_pkcs12_pbe(
    algorithm: &p12_q3::AlgorithmIdentifier,
    ciphertext: &[u8],
    password: &p12_q3::BmpString,
) -> Option<Vec<u8>> {
    let legacy = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        algorithm.decrypt_pbe(ciphertext, password)
    }))
    .ok()
    .flatten();
    if legacy.is_some() {
        return legacy;
    }

    if let Some(plaintext) = decrypt_pkcs12_aes_gcm(algorithm, ciphertext, password) {
        return Some(plaintext);
    }

    let algorithm_der = pkcs5_algorithm_der(algorithm)?;
    let scheme = pkcs5::EncryptionScheme::try_from(algorithm_der.as_slice()).ok()?;
    scheme.decrypt(password.as_ref(), ciphertext).ok()
}

pub(super) fn decrypt_pkcs12_aes_gcm(
    algorithm: &p12_q3::AlgorithmIdentifier,
    ciphertext: &[u8],
    password: &p12_q3::BmpString,
) -> Option<Vec<u8>> {
    let p12_q3::AlgorithmIdentifier::Pbes2(params) = algorithm else {
        return None;
    };
    let p12_q3::AlgorithmIdentifier::Pbkdf2(kdf) = params.key_derivation_function.as_ref() else {
        return None;
    };
    let p12_q3::Pbkdf2Salt::Specified(salt) = &kdf.salt else {
        return None;
    };

    let (key_len, nonce, tag_len) = match params.encryption_scheme.as_ref() {
        p12_q3::AlgorithmIdentifier::OtherAlg(other)
            if other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 6])
                || other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 46]) =>
        {
            let nonce = yasna::parse_der(other.params.as_deref()?, |reader| {
                reader.read_sequence(|reader| {
                    let nonce = reader.next().read_bytes()?;
                    let tag_length = reader
                        .read_optional(|reader| reader.read_u64())?
                        .unwrap_or(12);
                    Ok((nonce, tag_length))
                })
            })
            .ok()?;
            if !matches!(nonce.1, 12 | 16) || nonce.0.len() != 12 {
                return None;
            }
            let key_len = if other.algorithm_type == pkcs12_oid(&[2, 16, 840, 1, 101, 3, 4, 1, 6]) {
                16
            } else {
                32
            };
            (key_len, nonce.0, nonce.1 as usize)
        }
        _ => return None,
    };

    if kdf
        .key_length
        .is_some_and(|length| length as usize != key_len)
    {
        return None;
    }
    let iterations = u32::try_from(kdf.iteration_count).ok()?;
    let mut key = vec![0u8; key_len];
    match kdf.prf.as_ref() {
        p12_q3::AlgorithmIdentifier::HmacWithSha1 => {
            pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_ref(), salt, iterations, &mut key)
        }
        p12_q3::AlgorithmIdentifier::HmacWithSha256 => {
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_ref(), salt, iterations, &mut key)
        }
        p12_q3::AlgorithmIdentifier::OtherAlg(other)
            if is_pkcs5_hmac_prf(&other.algorithm_type)
                && has_der_null_parameters(other.params.as_deref()) =>
        {
            match other.algorithm_type.components().as_slice() {
                [1, 2, 840, 113549, 2, 8] => pbkdf2::pbkdf2_hmac::<sha2::Sha224>(
                    password.as_ref(),
                    salt,
                    iterations,
                    &mut key,
                ),
                [1, 2, 840, 113549, 2, 10] => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(
                    password.as_ref(),
                    salt,
                    iterations,
                    &mut key,
                ),
                [1, 2, 840, 113549, 2, 11] => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(
                    password.as_ref(),
                    salt,
                    iterations,
                    &mut key,
                ),
                _ => return None,
            }
        }
        _ => return None,
    }

    let (message, tag) = ciphertext.split_at_checked(ciphertext.len().checked_sub(tag_len)?)?;
    use aes_gcm::aead::consts::U12;
    use aes_gcm::aead::{AeadInPlace, KeyInit};
    if key_len == 16 && tag_len == 12 {
        let cipher =
            aes_gcm::AesGcm::<aes_gcm::aes::Aes128, U12, U12>::new_from_slice(&key).ok()?;
        let tag = aes_gcm::aead::generic_array::GenericArray::<u8, U12>::from_slice(tag);
        let mut plaintext = message.to_vec();
        cipher
            .decrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut plaintext, tag)
            .ok()?;
        Some(plaintext)
    } else if key_len == 32 && tag_len == 12 {
        let cipher =
            aes_gcm::AesGcm::<aes_gcm::aes::Aes256, U12, U12>::new_from_slice(&key).ok()?;
        let tag = aes_gcm::aead::generic_array::GenericArray::<u8, U12>::from_slice(tag);
        let mut plaintext = message.to_vec();
        cipher
            .decrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut plaintext, tag)
            .ok()?;
        Some(plaintext)
    } else if key_len == 16 {
        let cipher = aes_gcm::Aes128Gcm::new_from_slice(&key).ok()?;
        let tag = aes_gcm::Tag::from_slice(tag);
        let mut plaintext = message.to_vec();
        cipher
            .decrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut plaintext, tag)
            .ok()?;
        Some(plaintext)
    } else {
        let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).ok()?;
        let tag = aes_gcm::Tag::from_slice(tag);
        let mut plaintext = message.to_vec();
        cipher
            .decrypt_in_place_detached(aes_gcm::Nonce::from_slice(&nonce), &[], &mut plaintext, tag)
            .ok()?;
        Some(plaintext)
    }
}

pub(super) fn passwordless_pkcs12_bags(
    pfx: &p12_q3::PFX,
    password: &p12_q3::BmpString,
) -> Option<Vec<p12_q3::SafeBag>> {
    let authenticated_safe = match &pfx.auth_safe {
        p12_q3::ContentInfo::Data(data) => data.clone(),
        p12_q3::ContentInfo::EncryptedData(encrypted) => decrypt_pkcs12_pbe(
            &encrypted
                .encrypted_content_info
                .content_encryption_algorithm,
            &encrypted.encrypted_content_info.encrypted_content,
            password,
        )?,
        p12_q3::ContentInfo::OtherContext(_) => return None,
    };
    let contents = yasna::parse_ber(&authenticated_safe, |reader| {
        reader.collect_sequence_of(p12_q3::ContentInfo::parse)
    })
    .ok()?;

    let mut bags = Vec::new();
    for content in contents {
        let data = match content {
            p12_q3::ContentInfo::Data(data) => data,
            p12_q3::ContentInfo::EncryptedData(encrypted) => decrypt_pkcs12_pbe(
                &encrypted
                    .encrypted_content_info
                    .content_encryption_algorithm,
                &encrypted.encrypted_content_info.encrypted_content,
                password,
            )?,
            p12_q3::ContentInfo::OtherContext(_) => return None,
        };
        let safe_bags = yasna::parse_ber(&data, |reader| {
            reader.collect_sequence_of(p12_q3::SafeBag::parse)
        })
        .ok()?;
        bags.extend(safe_bags);
    }
    Some(bags)
}

pub(super) fn is_pkcs12_private_key_bag(bag: &p12_q3::SafeBag) -> bool {
    match &bag.bag {
        p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(_) => true,
        p12_q3::SafeBagKind::OtherBagKind(other) => {
            other.bag_id == pkcs12_oid(&[1, 2, 840, 113549, 1, 12, 10, 1, 1])
        }
        _ => false,
    }
}

fn decrypt_pkcs12_private_key(
    bag: &p12_q3::SafeBag,
    password: &p12_q3::BmpString,
) -> Option<Vec<u8>> {
    match &bag.bag {
        p12_q3::SafeBagKind::Pkcs8ShroudedKeyBag(key_bag) => decrypt_pkcs12_pbe(
            &key_bag.encryption_algorithm,
            &key_bag.encrypted_data,
            password,
        ),
        p12_q3::SafeBagKind::OtherBagKind(other)
            if other.bag_id == pkcs12_oid(&[1, 2, 840, 113549, 1, 12, 10, 1, 1]) =>
        {
            Some(other.bag_value.clone())
        }
        _ => None,
    }
}

pub(super) fn empty_pkcs12_password(pfx: &p12_q3::PFX) -> Option<p12_q3::BmpString> {
    let with_trailing_zeros = p12_q3::BmpString::with_two_trailing_zeros("");
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pfx.verify_mac(&with_trailing_zeros)
    }))
    .unwrap_or(false)
    {
        return Some(with_trailing_zeros);
    }

    let without_trailing_zeros = p12_q3::BmpString::empty_without_trailing_zeros();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pfx.verify_mac(&without_trailing_zeros)
    }))
    .unwrap_or(false)
    .then_some(without_trailing_zeros)
}

fn pem_block(label: &str, der: &[u8]) -> Vec<u8> {
    use base64::Engine;

    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n").into_bytes();
    for chunk in encoded.as_bytes().chunks(64) {
        pem.extend_from_slice(chunk);
        pem.push(b'\n');
    }
    pem.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    pem
}
