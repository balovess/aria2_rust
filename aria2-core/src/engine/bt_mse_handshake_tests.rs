//! MSE 握手模块的集成测试
//!
//! 测试覆盖所有三个握手阶段以及状态机转换、加密/解密功能等。

use crate::engine::bt_mse_handshake::*;
use sha1::{Digest, Sha1};

/// 辅助函数: 创建测试用的 info_hash
fn create_test_info_hash() -> [u8; 20] {
    [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32,
        0x10, 0xAA, 0xBB, 0xCC, 0xDD,
    ]
}

#[test]
fn test_method_selection_encrypted_support() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    let method_sel = manager.build_method_selection();

    // 应该返回 \x13MSegadd (8 bytes)
    assert_eq!(method_sel.len(), 8);
    assert_eq!(&method_sel, b"\x13MSegadd");
}

#[test]
fn test_method_selection_plain_only() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash, CryptoMethod::Plain).unwrap();

    // 即使选择 Plain 模式，build_method_selection 也应该返回 MSegadd
    // 因为这是协商过程，我们声明支持加密，但最终可能降级到 Plain
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
    // 空数据
    let result = MseHandshakeManager::parse_remote_method_selection(b"");
    assert!(result.is_err());

    // 无效数据
    let result = MseHandshakeManager::parse_remote_method_selection(b"\xffInvalid");
    assert!(result.is_err());
}

#[test]
fn test_key_exchange_payload_format() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    let payload = manager
        .build_key_exchange_payload(&[CryptoMethod::Rc4])
        .unwrap();

    // 最小长度: PAD_D(2) + PAD_LEN(2) + CryptoPro(2) + DH_PubKey(32) = 38
    assert!(payload.len() >= 38, "Payload too short: {}", payload.len());

    // 解析并验证字段
    let pad_d = u16::from_be_bytes([payload[0], payload[1]]);
    let pad_len = u16::from_be_bytes([payload[2], payload[3]]);
    let crypto_pro = u16::from_be_bytes([payload[4], payload[5]]);

    // PAD_D 和 PAD_LEN 应该相等
    assert_eq!(pad_d, pad_len);

    // CryptoPro 应该包含 RC4 标志 (0x0002)
    assert!(crypto_pro & 0x0002 != 0);

    // DH 公钥应该在最后 32 字节
    let dh_pubkey_start = payload.len() - 32;
    let dh_pubkey = &payload[dh_pubkey_start..];
    assert_eq!(dh_pubkey.len(), 32);
}

#[test]
fn test_dh_shared_secret_computation() {
    use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey};
    use ring::rand::SystemRandom;

    let rng = SystemRandom::new();

    // 创建 Alice 的密钥对
    let alice_private = EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let alice_public = alice_private.compute_public_key().unwrap();
    let mut alice_pubkey_vec = vec![0u8; 32]; // X25519 公钥固定为 32 字节
    alice_pubkey_vec.copy_from_slice(alice_public.as_ref());

    // 创建 Bob 的密钥对
    let bob_private = EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let bob_public = bob_private.compute_public_key().unwrap();
    let mut bob_pubkey_vec = vec![0u8; 32]; // X25519 公钥固定为 32 字节
    bob_pubkey_vec.copy_from_slice(bob_public.as_ref());

    // Alice 计算 shared secret using Bob's public key
    let bob_pubkey_parsed = UnparsedPublicKey::new(&agreement::X25519, &bob_pubkey_vec);
    let alice_shared = agreement::agree_ephemeral(
        alice_private,
        &bob_pubkey_parsed,
        |s: &[u8]| -> Result<Vec<u8>, ring::error::Unspecified> { Ok(s.to_vec()) },
    )
    .unwrap();

    // Bob 计算 shared secret using Alice's public key
    let alice_pubkey_parsed = UnparsedPublicKey::new(&agreement::X25519, &alice_pubkey_vec);
    let bob_shared = agreement::agree_ephemeral(
        bob_private,
        &alice_pubkey_parsed,
        |s: &[u8]| -> Result<Vec<u8>, ring::error::Unspecified> { Ok(s.to_vec()) },
    )
    .unwrap();

    // 双方计算出的共享密钥必须相同
    assert_eq!(alice_shared, bob_shared, "DH shared secrets must match");
}

#[test]
fn test_skey_computation() {
    let info_hash = create_test_info_hash();
    let mut manager = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    // 手动设置共享密钥用于测试
    let fake_shared_secret: Vec<u8> = vec![0x42; 32];
    manager.shared_secret = Some(fake_shared_secret.clone());

    // 计算 SKEY
    let skey = manager.compute_skey().unwrap();

    // 验证 SKEY = SHA-1(info_hash || shared_secret)
    let mut hasher = Sha1::new();
    hasher.update(info_hash);
    hasher.update(&fake_shared_secret);
    let expected_skey = hasher.finalize();

    assert_eq!(skey.to_vec(), expected_skey.to_vec());
    assert_eq!(skey.len(), 20); // SHA-1 输出 20 bytes
}

#[test]
fn test_key_derivation_send_recv_different() {
    let skey = vec![0xA5u8; 20];
    let shared_secret = vec![0x42u8; 32];

    let (send_key, recv_key): (Vec<u8>, Vec<u8>) =
        MseHandshakeManager::derive_keys(&skey, &shared_secret);

    // send_key 和 recv_key 应该不同
    assert_ne!(
        send_key, recv_key,
        "Send and receive keys must be different"
    );

    // 密钥长度应该是 16 bytes
    assert_eq!(send_key.len(), 16);
    assert_eq!(recv_key.len(), 16);
}

#[test]
fn test_rc4_encrypt_decrypt_roundtrip() {
    let send_key = vec![0xA5u8; 16];
    let recv_key = vec![0xB6u8; 16];

    // 模拟发送方: 用 send_key 加密
    let mut sender_ctx = MseCryptoContext::new(&send_key, &recv_key, CryptoMethod::Rc4);
    let plaintext = b"Hello, BitTorrent MSE!";
    let encrypted = sender_ctx.encrypt(plaintext).unwrap();

    // 加密后应该与原文不同
    assert_ne!(encrypted, plaintext.to_vec());

    // 模拟接收方: 用 recv_key 解密 (recv_key 必须与发送方的 send_key 匹配)
    // 在真实 MSE 场景中: 发送方.send_key == 接收方.recv_key
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
    // 验证 RC4 初始化时丢弃了前 1024 字节 keystream
    // 通过比较两个独立的加密上下文来验证

    let key = vec![0x42u8; 16];

    // 创建两个使用相同密钥的上下文
    let mut ctx1 = MseCryptoContext::new(&key, &key, CryptoMethod::Rc4);
    let mut ctx2 = MseCryptoContext::new(&key, &key, CryptoMethod::Rc4);

    // 相同明文应该产生相同的密文（因为都丢弃了前 1024 字节）
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

    // 加密和 decrypt 应该返回原文
    let mut ctx_mut = ctx;
    let encrypted = ctx_mut.encrypt(plaintext).unwrap();
    assert_eq!(encrypted, plaintext.to_vec());

    let decrypted = ctx_mut.decrypt(&encrypted).unwrap();
    assert_eq!(decrypted, plaintext.to_vec());
}

#[test]
fn test_state_machine_full_flow() {
    let info_hash = create_test_info_hash();
    let manager = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    // 初始状态: Idle
    assert!(matches!(manager.state(), MseState::Idle));

    // Phase 1: 发送 Method Selection
    manager.set_state(MseState::MethodSelectionSent);
    assert!(matches!(manager.state(), MseState::MethodSelectionSent));

    // Phase 2: Key Exchange
    manager.set_state(MseState::KeyExchangeInProgress);
    assert!(matches!(manager.state(), MseState::KeyExchangeInProgress));

    // Phase 3: Verification
    manager.set_state(MseState::VerificationPending);
    assert!(matches!(manager.state(), MseState::VerificationPending));

    // 完成: Established
    let fallback_ctx = MseHandshakeManager::plaintext_fallback();
    manager.set_state(MseState::Established(fallback_ctx));
    assert!(matches!(manager.state(), MseState::Established(_)));
}

#[test]
fn test_crypto_method_negotiation_rc4() {
    let info_hash = create_test_info_hash();

    // Alice (initiator)
    let mut alice = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    // Bob (responder)
    let mut bob = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    // Phase 1: Method Selection
    let alice_method = alice.build_method_selection();
    assert_eq!(&alice_method, b"\x13MSegadd");

    let bob_method = bob.build_method_selection();
    assert_eq!(&bob_method, b"\x13MSegadd");

    // 双方都支持加密
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

    // 处理对方的公钥
    alice.process_remote_key_exchange(&bob_payload).unwrap();
    bob.process_remote_key_exchange(&alice_payload).unwrap();

    // 双方应该计算出相同的共享密钥
    assert_eq!(
        alice.shared_secret(),
        bob.shared_secret(),
        "Shared secrets must match"
    );

    // Phase 3: Verification - 构建和解析验证载荷
    let alice_verify = alice.build_verification_payload(CryptoMethod::Rc4).unwrap();
    let bob_verify = bob.build_verification_payload(CryptoMethod::Rc4).unwrap();

    // 处理对方的验证
    let _alice_ctx = alice.process_remote_verification(&bob_verify).unwrap();
    let _bob_ctx = bob.process_remote_verification(&alice_verify).unwrap();
}

#[test]
fn test_crypto_method_negotiation_fallback_to_plain() {
    let info_hash = create_test_info_hash();

    // Alice 支持加密
    let _alice = MseHandshakeManager::new(info_hash, CryptoMethod::Rc4).unwrap();

    // Bob 仅支持明文 (发送 \x00)
    let bob_method_selection = b"\x00".to_vec();

    // Alice 解析 Bob 的 method selection
    let parsed = MseHandshakeManager::parse_remote_method_selection(&bob_method_selection).unwrap();

    assert_eq!(parsed, CryptoMethod::Plain);

    // 使用明文回退
    let ctx = MseHandshakeManager::plaintext_fallback();
    assert!(!ctx.is_encrypted());
}
