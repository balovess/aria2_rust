use super::*;

#[test]
fn test_arc4_encrypt_decrypt_roundtrip() {
    let key = b"test_key_123";
    let mut cipher1 = Arc4Cipher::new(key);
    let mut cipher2 = Arc4Cipher::new(key);

    let original = b"Hello, BitTorrent MSE!";
    let mut encrypted = original.to_vec();

    cipher1.encrypt(&mut encrypted);
    assert_ne!(encrypted, original.to_vec(), "encrypted should differ");

    cipher2.decrypt(&mut encrypted);
    assert_eq!(
        encrypted,
        original.to_vec(),
        "decrypted should match original"
    );
}

#[test]
fn test_arc4_symmetric() {
    let key = b"symmetric_key";
    let mut cipher = Arc4Cipher::new(key);

    let original = b"test_data";
    let mut data1 = original.to_vec();
    let mut data2 = original.to_vec();

    cipher.encrypt(&mut data1);
    let mut cipher2 = Arc4Cipher::new(key);
    cipher2.decrypt(&mut data2);

    assert_eq!(data1, data2);
}

#[test]
fn test_arc4_different_keys_different_output() {
    let mut cipher1 = Arc4Cipher::new(b"key1");
    let mut cipher2 = Arc4Cipher::new(b"key2");

    let original = b"same_plaintext";
    let mut enc1 = original.to_vec();
    let mut enc2 = original.to_vec();

    cipher1.encrypt(&mut enc1);
    cipher2.encrypt(&mut enc2);

    assert_ne!(enc1, enc2);
}

#[test]
fn test_derive_keys_deterministic() {
    let secret = [0xAAu8; KEY_LENGTH];
    let info_hash = [0xBBu8; INFO_HASH_LENGTH];

    let keys1 = MseDerivedKeys::derive(&secret, &info_hash);
    let keys2 = MseDerivedKeys::derive(&secret, &info_hash);

    assert_eq!(keys1.key_a, keys2.key_a);
    assert_eq!(keys1.key_b, keys2.key_b);
    assert_eq!(keys1.req1, keys2.req1);
    assert_eq!(keys1.req2, keys2.req2);
    assert_eq!(keys1.req3, keys2.req3);
}

#[test]
fn test_derive_keys_matches_cpp_format() {
    // Verify the key derivation input format matches C++:
    // keyA = SHA1("keyA" || S || infoHash)
    let secret = [0x42u8; KEY_LENGTH];
    let info_hash = [0x17u8; INFO_HASH_LENGTH];

    let keys = MseDerivedKeys::derive(&secret, &info_hash);

    // Manually compute key_a to verify
    let mut input = Vec::new();
    input.extend_from_slice(b"keyA");
    input.extend_from_slice(&secret);
    input.extend_from_slice(&info_hash);
    let expected = sha1_digest_array(&input);

    assert_eq!(keys.key_a, expected);
}

#[test]
fn test_derive_keys_are_full_20_bytes() {
    let secret = [0xAAu8; KEY_LENGTH];
    let info_hash = [0xBBu8; INFO_HASH_LENGTH];

    let keys = MseDerivedKeys::derive(&secret, &info_hash);

    // All keys should be full 20-byte SHA-1 digests, not truncated to 16
    assert_eq!(keys.key_a.len(), SHA1_LENGTH);
    assert_eq!(keys.key_b.len(), SHA1_LENGTH);
    assert_eq!(keys.req1.len(), SHA1_LENGTH);
    assert_eq!(keys.req2.len(), SHA1_LENGTH);
    assert_eq!(keys.req3.len(), SHA1_LENGTH);
}

#[test]
fn test_req2_xor_req3() {
    let secret = [0xAAu8; KEY_LENGTH];
    let info_hash = [0xBBu8; INFO_HASH_LENGTH];

    let keys = MseDerivedKeys::derive(&secret, &info_hash);
    let xor = keys.req2_xor_req3();

    let mut expected = [0u8; SHA1_LENGTH];
    for (expected, (&req2, &req3)) in expected
        .iter_mut()
        .zip(keys.req2.iter().zip(keys.req3.iter()))
    {
        *expected = req2 ^ req3;
    }
    assert_eq!(xor, expected);
}

#[test]
fn test_crypto_method_roundtrip() {
    assert_eq!(MseCryptoMethod::from_u32(0x01), MseCryptoMethod::Plain);
    assert_eq!(MseCryptoMethod::from_u32(0x02), MseCryptoMethod::Rc4);
    assert_eq!(MseCryptoMethod::from_u32(0x03), MseCryptoMethod::Rc4); // bit 0x02 set
    assert_eq!(MseCryptoMethod::Rc4.as_u32(), 0x02);
    assert_eq!(MseCryptoMethod::Plain.as_u32(), 0x01);
}

#[test]
fn test_rc4_encrypt_decrypt_roundtrip_via_state() {
    let secret = [0xCCu8; KEY_LENGTH];
    let info_hash = [0xDDu8; INFO_HASH_LENGTH];
    let keys = MseDerivedKeys::derive(&secret, &info_hash);

    let mut crypto_initiator = MseCryptoState::new_encrypted(&keys, true);
    let mut crypto_responder = MseCryptoState::new_encrypted(&keys, false);

    let original = b"Hello, BitTorrent MSE!";
    let mut encrypted = original.to_vec();

    crypto_initiator.encrypt(&mut encrypted);
    assert_ne!(encrypted, original.to_vec());

    crypto_responder.decrypt(&mut encrypted);
    assert_eq!(encrypted, original.to_vec());
}

#[test]
fn test_plain_mode_noop() {
    let mut crypto = MseCryptoState::new_plain();
    let mut data = b"unchanged".to_vec();

    crypto.encrypt(&mut data);
    crypto.decrypt(&mut data);

    assert_eq!(data, b"unchanged".to_vec());
}

#[test]
fn test_vc_is_rc4_encrypted_zeros() {
    // VC = RC4(8 zero bytes), NOT SHA1-based
    let key = [0x42u8; SHA1_LENGTH];
    let mut cipher = init_rc4(&key);
    let vc = compute_vc(&mut cipher);

    // VC should not be all zeros (RC4 of zeros is not zeros)
    assert_ne!(
        vc, [0u8; VC_LENGTH],
        "VC should not be all zeros after RC4 encryption"
    );

    // Verify VC is deterministic for same key
    let mut cipher2 = init_rc4(&key);
    let vc2 = compute_vc(&mut cipher2);
    assert_eq!(vc, vc2, "VC should be deterministic for same key");
}

#[test]
fn test_different_secrets_different_keys() {
    let secret1 = [0x01u8; KEY_LENGTH];
    let secret2 = [0x02u8; KEY_LENGTH];
    let info_hash = [0xBBu8; INFO_HASH_LENGTH];

    let keys1 = MseDerivedKeys::derive(&secret1, &info_hash);
    let keys2 = MseDerivedKeys::derive(&secret2, &info_hash);

    assert_ne!(keys1.key_a, keys2.key_a);
    assert_ne!(keys1.key_b, keys2.key_b);
    assert_ne!(keys1.req1, keys2.req1);
}

#[test]
fn test_different_info_hashes_different_keys() {
    let secret = [0xAAu8; KEY_LENGTH];
    let info_hash1 = [0x01u8; INFO_HASH_LENGTH];
    let info_hash2 = [0x02u8; INFO_HASH_LENGTH];

    let keys1 = MseDerivedKeys::derive(&secret, &info_hash1);
    let keys2 = MseDerivedKeys::derive(&secret, &info_hash2);

    // key_a and key_b include infoHash, so they differ
    assert_ne!(keys1.key_a, keys2.key_a);
    assert_ne!(keys1.key_b, keys2.key_b);
    // req2 includes infoHash, so it differs
    assert_ne!(keys1.req2, keys2.req2);
    // req1 and req3 depend only on S, so they are the same
    assert_eq!(keys1.req1, keys2.req1);
    assert_eq!(keys1.req3, keys2.req3);
}

#[test]
fn test_multiple_messages_independent() {
    let secret = [0xEEu8; KEY_LENGTH];
    let info_hash = [0xFFu8; INFO_HASH_LENGTH];
    let keys = MseDerivedKeys::derive(&secret, &info_hash);

    let mut sender = MseCryptoState::new_encrypted(&keys, true);
    let mut receiver = MseCryptoState::new_encrypted(&keys, false);

    let msgs: &[&[u8]] = &[b"first", b"second message", b"third longer msg"];
    for msg in msgs {
        let mut enc = msg.to_vec();
        sender.encrypt(&mut enc);
        receiver.decrypt(&mut enc);
        assert_eq!(enc, *msg);
    }
}
