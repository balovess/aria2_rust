use aria2_core::checksum::message_digest::{HashType, MessageDigest};
use aria2_core::checksum::checksum::{Checksum, ChecksumValidator};
use aria2_core::checksum::chunk_checksum::ChunkChecksum;

#[test]
fn test_e2e_md5_known_vector_rfc1321() {
    let hex = MessageDigest::hash_hex(HashType::Md5, b"");
    assert_eq!(hex, "d41d8cd98f00b204e9800998ecf8427e");

    let hex = MessageDigest::hash_hex(HashType::Md5, b"a");
    assert_eq!(hex, "0cc175b9c0f1b6a831c399e269772661");

    let hex = MessageDigest::hash_hex(HashType::Md5, b"abc");
    assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");

    let hex = MessageDigest::hash_hex(HashType::Md5, b"message digest");
    assert_eq!(hex, "f96b697d7cb7938d525a2f31aaf161d0");
}

#[test]
fn test_e2e_sha1_fips180_4() {
    let hex = MessageDigest::hash_hex(HashType::Sha1, b"abc");
    assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");

    let hex = MessageDigest::hash_hex(HashType::Sha1,
        b"abcdbcdecdefdefgefghfghijkijklmnopqrstuvwxxyz");
    assert_eq!(hex, "b5ffe488358705f24ebaf43727d8f4d413480d60");
}

#[test]
fn test_e2e_sha256_nist_vector() {
    let hex = MessageDigest::hash_hex(HashType::Sha256, b"");
    assert_eq!(hex, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");

    let hex = MessageDigest::hash_hex(HashType::Sha256, b"hello world");
    let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
    assert_eq!(hex, expected);
}

#[test]
fn test_e2e_adler32_basic() {
    let bytes = MessageDigest::hash_data(HashType::Adler32, b"hello world");
    assert_eq!(bytes.len(), 4);

    let empty = MessageDigest::hash_data(HashType::Adler32, b"");
    assert_ne!(empty.len(), 0);
}

#[test]
fn test_e2e_checksum_verify_correct_data() {
    let cs = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
    assert!(cs.verify(b""), "空字符串 MD5 应匹配");

    let cs = Checksum::new(HashType::Sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
    assert!(cs.verify(b"abc"), "'abc' SHA-256 应匹配");
}

#[test]
fn test_e2e_checksum_reject_wrong_data() {
    let cs = Checksum::new(HashType::Md5, "d41d8cd98f00b204e9800998ecf8427e").unwrap();
    assert!(!cs.verify(b"not empty"), "非空数据不应通过 MD5 空校验");

    let cs = Checksum::new(HashType::Sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709").unwrap();
    assert!(!cs.verify(b"x"), "单字符 'x' 不应通过 SHA1 空校验");
}

#[test]
fn test_e2e_checksum_from_type_string_parsing() {
    let cases = vec![
        ("md5", HashType::Md5),
        ("MD5", HashType::Md5),
        ("sha-1", HashType::Sha1),
        ("SHA1", HashType::Sha1),
        ("sha-256", HashType::Sha256),
        ("sha256", HashType::Sha256),
        ("sha-512", HashType::Sha512),
        ("adler32", HashType::Adler32),
    ];
    for (str_repr, expected) in &cases {
        let ht = HashType::from_str(str_repr).expect(&format!("{} 应被解析", str_repr));
        assert_eq!(ht, *expected);
    }
}

#[test]
fn test_e2e_checksum_invalid_inputs_rejected() {
    assert!(Checksum::new(HashType::Md5, "").is_err(), "空值应失败");
    assert!(Checksum::new(HashType::Md5, "zzz").is_err(), "非法 hex 应失败");
    assert!(Checksum::new(HashType::Md5, "abcd").is_err(), "长度不足应失败 (MD5=32 hex)");
    assert!(Checksum::from_type_and_value("blake3", "abc").is_err(), "未知算法应失败");
}

#[test]
fn test_e2e_chunk_checksum_multi_piece() {
    let data1 = vec![0xAAu8; 1024];
    let data2 = vec![0xBBu8; 1024];
    let data3 = vec![0xCCu8; 100];

    let hashes = vec![
        MessageDigest::hash_hex(HashType::Sha1, &data1),
        MessageDigest::hash_hex(HashType::Sha1, &data2),
        MessageDigest::hash_hex(HashType::Sha1, &data3),
    ];

    let cc = ChunkChecksum::new(HashType::Sha1, hashes, 1024);
    assert_eq!(cc.piece_count(), 3);
    assert!(cc.verify_chunk(&data1, 0));
    assert!(cc.verify_chunk(&data2, 1));
    assert!(cc.verify_chunk(&data3, 2));
    assert!(!cc.verify_chunk(&[0xFF; 1024], 0));
}

#[test]
fn test_e2e_chunk_checksum_partial_verify() {
    let cc = ChunkChecksum::new(
        HashType::Sha256,
        vec![
            MessageDigest::hash_hex(HashType::Sha256, &[1u8; 16384]),
            MessageDigest::hash_hex(HashType::Sha256, &[2u8; 16384]),
            MessageDigest::hash_hex(HashType::Sha256, &[3u8; 500]),
        ],
        16384,
    );

    assert!(cc.verify_chunk(&[1u8; 16384], 0));
    assert!(cc.verify_chunk(&[3u8; 500], 2));
    assert!(!cc.verify_chunk(&[0u8; 500], 2));
    assert!(!cc.verify_chunk(&[0u8; 16384], 5));
}

#[test]
fn test_e2e_streaming_validator_consistency() {
    let cs = Checksum::new(HashType::Sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    ).unwrap();

    let mut validator = cs.create_validator();
    validator.update(b"ab");
    validator.update(b"c");
    assert!(validator.finalize().unwrap());

    assert!(cs.verify(b"abc"), "流式验证结果应与一次性验证一致");
}

#[test]
fn test_e2e_hash_type_digest_length_consistency() {
    let lengths: Vec<(HashType, usize)> = vec![
        (HashType::Md5, 16),
        (HashType::Sha1, 20),
        (HashType::Sha256, 32),
        (HashType::Sha512, 64),
        (HashType::Adler32, 4),
    ];
    for (ht, expected_len) in &lengths {
        assert_eq!(ht.digest_length(), *expected_len, "{:?} 长度不匹配", ht);
        let md = MessageDigest::new(*ht);
        assert_eq!(md.digest_length(), *expected_len);
    }
}

#[test]
fn test_e2e_large_data_hashing() {
    let large_data = vec![0x42u8; 100_000];
    let hash1 = MessageDigest::hash_hex(HashType::Sha256, &large_data);

    let mut streaming = MessageDigest::new(HashType::Sha256);
    for chunk in large_data.chunks(4096) {
        streaming.update(chunk);
    }
    let hash2 = streaming.finalize_hex();

    assert_eq!(hash1, hash2, "大数据流式和一次性哈希应一致");
}
