use super::*;

fn test_info_hash() -> [u8; INFO_HASH_LENGTH] {
    [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54, 0x32,
        0x10, 0xAA, 0xBB, 0xCC, 0xDD,
    ]
}

#[test]
fn test_step1_payload_size() {
    let h = MseHandshake::new_initiator(test_info_hash());
    let s1 = h.build_step1();
    assert!(s1.len() >= KEY_LENGTH);
    assert!(s1.len() <= KEY_LENGTH + MAX_PAD_LENGTH);
}

#[test]
fn test_receive_step1_too_short() {
    let mut h = MseHandshake::new_responder(test_info_hash());
    assert!(h.receive_step1(&[0u8; 16]).is_err());
}

#[test]
fn test_receive_step1_computes_shared_secret() {
    let initiator = MseHandshake::new_initiator(test_info_hash());
    let step1 = initiator.build_step1();

    let mut responder = MseHandshake::new_responder(test_info_hash());
    responder.receive_step1(&step1).expect("receive step1");

    assert!(responder.shared_secret.is_some());
    assert!(responder.keys.is_some());
}

#[test]
fn test_full_handshake_rc4() {
    let info_hash = test_info_hash();
    let mut initiator = MseHandshake::new_initiator(info_hash);
    let mut responder = MseHandshake::new_responder(info_hash);

    let i_step1 = initiator.build_step1();
    let r_step1 = responder.build_step1();
    initiator
        .receive_step1(&r_step1)
        .expect("I receive R step1");
    responder
        .receive_step1(&i_step1)
        .expect("R receive I step1");
    assert_eq!(initiator.shared_secret, responder.shared_secret);

    let i_step3 = initiator.build_initiator_step2().expect("I build step3");
    assert_eq!(
        responder
            .receive_initiator_step2(&i_step3, &[info_hash])
            .expect("R receive step3"),
        MseCryptoMethod::Rc4
    );

    let r_step4 = responder.build_receiver_step2().expect("R build step4");
    assert_eq!(
        initiator
            .receive_receiver_step2(&r_step4)
            .expect("I receive step4"),
        MseCryptoMethod::Rc4
    );
    let mut crypto_i = initiator.finalize().expect("I finalize");
    let mut crypto_r = responder.finalize().expect("R finalize");
    assert!(crypto_i.is_encrypted());
    assert!(crypto_r.is_encrypted());

    let original = b"Test encrypted data";
    let mut encrypted = original.to_vec();
    crypto_i.encrypt(&mut encrypted);
    assert_ne!(encrypted, original);
    crypto_r.decrypt(&mut encrypted);
    assert_eq!(encrypted, original);
}

#[test]
fn test_receiver_response_with_plain_pad_b_prefix() {
    let info_hash = test_info_hash();
    let mut initiator = MseHandshake::new_initiator(info_hash);
    let mut responder = MseHandshake::new_responder(info_hash);
    let i_step1 = initiator.build_step1();
    let r_step1 = responder.build_step1();
    initiator.receive_step1(&r_step1).unwrap();
    responder.receive_step1(&i_step1).unwrap();
    let i_step3 = initiator.build_initiator_step2().unwrap();
    responder
        .receive_initiator_step2(&i_step3, &[info_hash])
        .unwrap();
    let r_step4 = responder.build_receiver_step2().unwrap();

    let mut buffered_response = vec![0xA5; 17];
    buffered_response.extend_from_slice(&r_step4);
    let required = initiator
        .initiator_step2_required_len(&buffered_response)
        .unwrap()
        .unwrap();
    assert_eq!(required, buffered_response.len());
    initiator
        .receive_receiver_step2(&buffered_response)
        .unwrap();
    let mut crypto_i = initiator.finalize().unwrap();
    let mut crypto_r = responder.finalize().unwrap();

    let mut handshake = [0x5Au8; 68];
    crypto_i.encrypt(&mut handshake);
    crypto_r.decrypt(&mut handshake);
    assert_eq!(handshake, [0x5Au8; 68]);

    let mut reverse = [0x6Bu8; 68];
    crypto_r.encrypt(&mut reverse);
    crypto_i.decrypt(&mut reverse);
    assert_eq!(reverse, [0x6Bu8; 68]);
}

#[test]
fn test_full_wire_flow_retains_pad_a_and_pad_b() {
    let info_hash = test_info_hash();
    let mut initiator = MseHandshake::new_initiator(info_hash);
    let mut responder = MseHandshake::new_responder([0u8; 20]);
    let initiator_step1 = initiator.build_step1();
    let responder_step1 = responder.build_step1();

    initiator
        .receive_step1(&responder_step1[..KEY_LENGTH])
        .unwrap();
    responder
        .receive_step1(&initiator_step1[..KEY_LENGTH])
        .unwrap();

    let initiator_step3 = initiator.build_initiator_step2().unwrap();
    let mut responder_buffer = initiator_step1[KEY_LENGTH..].to_vec();
    responder_buffer.extend_from_slice(&initiator_step3);
    let discovered = responder
        .receiver_info_hash(&responder_buffer, &[info_hash])
        .unwrap()
        .unwrap();
    responder.set_info_hash(discovered).unwrap();
    let required = responder
        .receiver_step2_required_len(&responder_buffer)
        .unwrap()
        .unwrap();
    assert_eq!(required, responder_buffer.len());
    responder
        .receive_initiator_step2(&responder_buffer, &[info_hash])
        .unwrap();
    let responder_step4 = responder.build_receiver_step2().unwrap();

    let mut initiator_buffer = responder_step1[KEY_LENGTH..].to_vec();
    initiator_buffer.extend_from_slice(&responder_step4);
    let required = initiator
        .initiator_step2_required_len(&initiator_buffer)
        .unwrap()
        .unwrap();
    assert_eq!(required, initiator_buffer.len());
    initiator.receive_receiver_step2(&initiator_buffer).unwrap();
    let mut initiator_crypto = initiator.finalize().unwrap();
    let mut responder_crypto = responder.finalize().unwrap();

    let mut message = [0x77u8; 68];
    initiator_crypto.encrypt(&mut message);
    responder_crypto.decrypt(&mut message);
    assert_eq!(message, [0x77u8; 68]);

    let mut reverse = [0x88u8; 68];
    responder_crypto.encrypt(&mut reverse);
    initiator_crypto.decrypt(&mut reverse);
    assert_eq!(reverse, [0x88u8; 68]);
}

#[test]
fn test_handshake_type_detection() {
    let mut legacy = vec![19u8];
    legacy.extend_from_slice(b"BitTorrent protocol");
    assert_eq!(
        MseHandshake::identify_handshake_type(&legacy),
        HandshakeType::Legacy
    );

    let encrypted = [0u8; 20];
    assert_eq!(
        MseHandshake::identify_handshake_type(&encrypted),
        HandshakeType::Encrypted
    );
    let short = [19u8; 10];
    assert_eq!(
        MseHandshake::identify_handshake_type(&short),
        HandshakeType::NotYet
    );
}

#[test]
fn test_should_negotiate() {
    let reserved_all_zero = [0u8; 8];
    let mut reserved_mse_set = [0u8; 8];
    reserved_mse_set[7] = 0x01;

    assert!(!MseHandshake::should_negotiate(true, &reserved_all_zero));
    assert!(MseHandshake::should_negotiate(true, &reserved_mse_set));
    assert!(!MseHandshake::should_negotiate(false, &reserved_mse_set));
    assert!(!MseHandshake::should_negotiate(true, &[]));
}

#[test]
fn test_different_instances_different_keys() {
    let h1 = MseHandshake::new_initiator(test_info_hash());
    let h2 = MseHandshake::new_initiator(test_info_hash());
    assert_ne!(h1.dh.generate_public_key(), h2.dh.generate_public_key());
}

#[test]
fn test_initiator_step2_before_keys() {
    let mut h = MseHandshake::new_initiator(test_info_hash());
    assert!(h.build_initiator_step2().is_err());
}

#[test]
fn test_finalize_before_completed() {
    let h = MseHandshake::new_initiator(test_info_hash());
    assert!(h.finalize().is_err());
}

#[test]
fn test_receiver_step4_before_completed() {
    let mut h = MseHandshake::new_responder(test_info_hash());
    assert!(h.build_receiver_step2().is_err());
}

#[test]
fn test_only_initiator_builds_step3() {
    let mut h = MseHandshake::new_responder(test_info_hash());
    assert!(h.build_initiator_step2().is_err());
}

#[test]
fn test_only_receiver_builds_step4() {
    let mut h = MseHandshake::new_initiator(test_info_hash());
    assert!(h.build_receiver_step2().is_err());
}

#[test]
fn test_only_receiver_processes_step3() {
    let mut h = MseHandshake::new_initiator(test_info_hash());
    assert!(h.receive_initiator_step2(&[], &[]).is_err());
}

#[test]
fn test_only_initiator_processes_step4() {
    let mut h = MseHandshake::new_responder(test_info_hash());
    assert!(h.receive_receiver_step2(&[]).is_err());
}

#[test]
fn test_key_derivation_format() {
    let info_hash = test_info_hash();
    let initiator = MseHandshake::new_initiator(info_hash);
    let step1 = initiator.build_step1();

    let mut responder = MseHandshake::new_responder(info_hash);
    responder.receive_step1(&step1).expect("receive step1");
    let keys = responder.keys.as_ref().expect("keys derived");
    let shared = responder.shared_secret.expect("shared secret");
    let mut input = Vec::new();
    input.extend_from_slice(b"keyA");
    input.extend_from_slice(&shared);
    input.extend_from_slice(&info_hash);

    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(&input);
    let expected = hasher.finalize();
    assert_eq!(&keys.key_a[..], &expected[..]);
}

#[test]
fn test_multi_torrent_info_hash_selection() {
    let info_hash_1 = [0x01u8; INFO_HASH_LENGTH];
    let info_hash_2 = [0x02u8; INFO_HASH_LENGTH];
    let mut initiator = MseHandshake::new_initiator(info_hash_2);
    let mut responder = MseHandshake::new_responder(info_hash_1);

    let i_step1 = initiator.build_step1();
    let r_step1 = responder.build_step1();
    initiator
        .receive_step1(&r_step1)
        .expect("I receive R step1");
    responder
        .receive_step1(&i_step1)
        .expect("R receive I step1");
    let i_step3 = initiator.build_initiator_step2().expect("I build step3");
    assert_eq!(
        responder
            .receive_initiator_step2(&i_step3, &[info_hash_1, info_hash_2])
            .expect("R receive step3"),
        MseCryptoMethod::Rc4
    );
    assert_eq!(responder.info_hash, info_hash_2);
}
