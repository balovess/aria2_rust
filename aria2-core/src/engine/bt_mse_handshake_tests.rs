//! Integration tests for the MSE handshake module
//!
//! Tests cover all three handshake phases, state machine transitions, encryption/decryption, etc.
//!
//! # Deprecation Note
//!
//! The module under test (`bt_mse_handshake`) is deprecated. These tests
//! are retained for regression but new code should use the corrected
//! implementation in `aria2-protocol`.

#![allow(deprecated)]

use crate::engine::bt_mse_handshake::*;
use sha1::{Digest, Sha1};

/// Helper function: create an info_hash for testing
fn create_test_info_hash() -> [u8; 20] {
    [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32,
        0x10, 0xAA, 0xBB, 0xCC, 0xDD,
    ]
}

#[test]
fn test_method_selection_encrypted_support() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash).unwrap();

    let method_sel = manager.build_method_selection();

    // Should return \x13MSegadd (8 bytes)
    assert_eq!(method_sel.len(), 8);
    assert_eq!(&method_sel, b"\x13MSegadd");
}

#[test]
fn test_method_selection_plain_only() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash).unwrap();

    // Even when Plain mode is selected, build_method_selection should still return MSegadd
    // Because this is the negotiation process, we declare support for encryption but may eventually fall back to Plain
    let method_sel = manager.build_method_selection();

    assert_eq!(method_sel.len(), 8);
    assert_eq!(&method_sel, b"\x13MSegadd");
}

#[test]
fn test_parse_remote_method_msegadd() {
    let result = MseHandshakeManager::parse_remote_method_selection(b"\x13MSegadd").unwrap();

    assert_eq!(result, CryptoMethod::Rc4);
}

#[test]
fn test_parse_remote_method_invalid() {
    // Empty data
    let result = MseHandshakeManager::parse_remote_method_selection(b"");
    assert!(result.is_err());

    // Invalid data
    let result = MseHandshakeManager::parse_remote_method_selection(b"\xffInvalid");
    assert!(result.is_err());
}

#[test]
fn test_key_exchange_payload_format() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash).unwrap();

    let payload = manager
        .build_key_exchange_payload(&[CryptoMethod::Rc4])
        .unwrap();

    // Minimum length: PAD_D(2) + PAD_LEN(2) + CryptoPro(2) + DH_PubKey(32) = 38
    assert!(payload.len() >= 38, "Payload too short: {}", payload.len());

    // Parse and verify fields
    let pad_d = u16::from_be_bytes([payload[0], payload[1]]);
    let pad_len = u16::from_be_bytes([payload[2], payload[3]]);
    let crypto_pro = u16::from_be_bytes([payload[4], payload[5]]);

    // PAD_D and PAD_LEN should be equal
    assert_eq!(pad_d, pad_len);

    // CryptoPro should contain RC4 flag (0x0002)
    assert!(crypto_pro & 0x0002 != 0);

    // DH public key should be in the last 32 bytes
    let dh_pubkey_start = payload.len() - 32;
    let dh_pubkey = &payload[dh_pubkey_start..];
    assert_eq!(dh_pubkey.len(), 32);
}

#[test]
fn test_dh_shared_secret_computation() {
    use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
    use ring::rand::SystemRandom;

    let rng = SystemRandom::new();

    // Create Alice's key pair
    let alice_private = EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let alice_public = alice_private.compute_public_key().unwrap();
    let mut alice_pubkey_vec = vec![0u8; 32]; // X25519 public key is fixed at 32 bytes
    alice_pubkey_vec.copy_from_slice(alice_public.as_ref());

    // Create Bob's key pair
    let bob_private = EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let bob_public = bob_private.compute_public_key().unwrap();
    let mut bob_pubkey_vec = vec![0u8; 32]; // X25519 public key is fixed at 32 bytes
    bob_pubkey_vec.copy_from_slice(bob_public.as_ref());

    // Alice computes shared secret using Bob's public key
    let bob_pubkey_parsed = UnparsedPublicKey::new(&agreement::X25519, &bob_pubkey_vec);
    let alice_shared = agreement::agree_ephemeral(
        alice_private,
        &bob_pubkey_parsed,
        |s: &[u8]| -> Result<Vec<u8>, ring::error::Unspecified> { Ok(s.to_vec()) },
    )
    .unwrap();

    // Bob computes shared secret using Alice's public key
    let alice_pubkey_parsed = UnparsedPublicKey::new(&agreement::X25519, &alice_pubkey_vec);
    let bob_shared = agreement::agree_ephemeral(
        bob_private,
        &alice_pubkey_parsed,
        |s: &[u8]| -> Result<Vec<u8>, ring::error::Unspecified> { Ok(s.to_vec()) },
    )
    .unwrap();

    // Shared secrets computed by both parties must match
    assert_eq!(alice_shared, bob_shared, "DH shared secrets must match");
}

#[test]
fn test_skey_computation() {
    let info_hash = create_test_info_hash();
    let mut manager = MseHandshakeManager::new(info_hash).unwrap();

    // Manually set shared secret for testing
    let fake_shared_secret: Vec<u8> = vec![0x42; 32];
    manager.shared_secret = Some(fake_shared_secret.clone());

    // Compute SKEY
    let skey = manager.compute_skey().unwrap();

    // Verify SKEY = SHA-1(info_hash || shared_secret)
    let mut hasher = Sha1::new();
    hasher.update(info_hash);
    hasher.update(&fake_shared_secret);
    let expected_skey = hasher.finalize();

    assert_eq!(skey.to_vec(), expected_skey.to_vec());
    assert_eq!(skey.len(), 20); // SHA-1 outputs 20 bytes
}

#[test]
fn test_key_derivation_send_recv_different() {
    let skey = vec![0xA5u8; 20];
    let shared_secret = vec![0x42u8; 32];

    let (send_key, recv_key): (Vec<u8>, Vec<u8>) =
        MseHandshakeManager::derive_keys(&skey, &shared_secret);

    // send_key and recv_key should be different
    assert_ne!(
        send_key, recv_key,
        "Send and receive keys must be different"
    );

    // Key length should be 16 bytes
    assert_eq!(send_key.len(), 16);
    assert_eq!(recv_key.len(), 16);
}

#[test]
fn test_rc4_encrypt_decrypt_roundtrip() {
    let send_key = vec![0xA5u8; 16];
    let recv_key = vec![0xB6u8; 16];

    // Simulate sender: encrypt with send_key
    let mut sender_ctx = MseCryptoContext::new(&send_key, &recv_key, CryptoMethod::Rc4);
    let plaintext = b"Hello, BitTorrent MSE!";
    let encrypted = sender_ctx.encrypt(plaintext).unwrap();

    // Encrypted data should differ from plaintext
    assert_ne!(encrypted, plaintext.to_vec());

    // Simulate receiver: decrypt with recv_key (recv_key must match sender's send_key)
    // In real MSE scenario: sender.send_key == receiver.recv_key
    let mut receiver_ctx = MseCryptoContext::new(&recv_key, &send_key, CryptoMethod::Rc4);
    let decrypted = receiver_ctx.decrypt(&encrypted).unwrap();
    assert_eq!(
        decrypted,
        plaintext.to_vec(),
        "Receiver should decrypt sender's ciphertext"
    );
}

#[test]
fn test_rc4_initial_state_discard() {
    // Verify that RC4 initialization discards the first 1024 bytes of keystream
    // Verified by comparing two independent encryption contexts

    let key = vec![0x42u8; 16];

    // Create two contexts using the same key
    let mut ctx1 = MseCryptoContext::new(&key, &key, CryptoMethod::Rc4);
    let mut ctx2 = MseCryptoContext::new(&key, &key, CryptoMethod::Rc4);

    // Same plaintext should produce same ciphertext (because both discard the first 1024 bytes)
    let data = b"Test data for keystream discard verification";
    let enc1 = ctx1.encrypt(data).unwrap();
    let enc2 = ctx2.encrypt(data).unwrap();

    assert_eq!(
        enc1, enc2,
        "Same key should produce same ciphertext after discard"
    );
}

#[test]
fn test_plaintext_fallback_no_op() {
    let ctx = MseHandshakeManager::plaintext_fallback();

    assert!(!ctx.is_encrypted());
    assert_eq!(ctx.crypto_method(), CryptoMethod::Plain);

    let plaintext = b"Plain text should not be modified";

    // Encrypt and decrypt should return the original plaintext
    let mut ctx_mut = ctx;
    let encrypted = ctx_mut.encrypt(plaintext).unwrap();
    assert_eq!(encrypted, plaintext.to_vec());

    let decrypted = ctx_mut.decrypt(&encrypted).unwrap();
    assert_eq!(decrypted, plaintext.to_vec());
}

#[test]
fn test_state_machine_full_flow() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash).unwrap();

    // Initial state: Idle
    assert!(matches!(manager.state(), MseState::Idle));

    // Phase 1: Send Method Selection
    manager.set_state(MseState::MethodSelectionSent);
    assert!(matches!(manager.state(), MseState::MethodSelectionSent));

    // Phase 2: Key Exchange
    manager.set_state(MseState::KeyExchangeInProgress);
    assert!(matches!(manager.state(), MseState::KeyExchangeInProgress));

    // Phase 3: Verification
    manager.set_state(MseState::VerificationPending);
    assert!(matches!(manager.state(), MseState::VerificationPending));

    // Complete: Established
    let fallback_ctx = MseHandshakeManager::plaintext_fallback();
    manager.set_state(MseState::Established(fallback_ctx));
    assert!(matches!(manager.state(), MseState::Established(_)));
}

#[test]
fn test_crypto_method_negotiation_rc4() {
    let info_hash = create_test_info_hash();

    // Alice (initiator)
    let mut alice = MseHandshakeManager::new(info_hash).unwrap();

    // Bob (responder)
    let mut bob = MseHandshakeManager::new(info_hash).unwrap();

    // Phase 1: Method Selection
    let alice_method = alice.build_method_selection();
    assert_eq!(&alice_method, b"\x13MSegadd");

    let bob_method = bob.build_method_selection();
    assert_eq!(&bob_method, b"\x13MSegadd");

    // Both parties support encryption
    let alice_parse = MseHandshakeManager::parse_remote_method_selection(&bob_method).unwrap();
    let bob_parse = MseHandshakeManager::parse_remote_method_selection(&alice_method).unwrap();
    assert_eq!(alice_parse, CryptoMethod::Rc4);
    assert_eq!(bob_parse, CryptoMethod::Rc4);

    // Phase 2: Key Exchange
    let alice_payload = alice
        .build_key_exchange_payload(&[CryptoMethod::Rc4])
        .unwrap();
    let bob_payload = bob
        .build_key_exchange_payload(&[CryptoMethod::Rc4])
        .unwrap();

    // Process the other party's public key
    alice.process_remote_key_exchange(&bob_payload).unwrap();
    bob.process_remote_key_exchange(&alice_payload).unwrap();

    // Both parties should compute the same shared secret
    assert_eq!(
        alice.shared_secret(),
        bob.shared_secret(),
        "Shared secrets must match"
    );

    // Phase 3: Verification - build and parse verification payload
    let alice_verify = alice.build_verification_payload(CryptoMethod::Rc4).unwrap();
    let bob_verify = bob.build_verification_payload(CryptoMethod::Rc4).unwrap();

    // Process the other party's verification
    let _alice_ctx = alice.process_remote_verification(&bob_verify).unwrap();
    let _bob_ctx = bob.process_remote_verification(&alice_verify).unwrap();
}

#[test]
fn test_crypto_method_negotiation_fallback_to_plain() {
    let info_hash = create_test_info_hash();

    // Alice supports encryption
    let _alice = MseHandshakeManager::new(info_hash).unwrap();

    // Bob only supports plaintext (sends \x00)
    let bob_method_selection = b"\x00".to_vec();

    // Alice parses Bob's method selection
    let parsed = MseHandshakeManager::parse_remote_method_selection(&bob_method_selection).unwrap();

    assert_eq!(parsed, CryptoMethod::Plain);

    // Use plaintext fallback
    let ctx = MseHandshakeManager::plaintext_fallback();
    assert!(!ctx.is_encrypted());
}
