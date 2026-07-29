//! MSE (Message Stream Encryption) crypto primitives.
//!
//! Matches C++ aria2 `MSEHandshake.cc` key derivation:
//! - `keyA = SHA1("keyA" || S || infoHash)` — full 20-byte RC4 key
//! - `keyB = SHA1("keyB" || S || infoHash)` — full 20-byte RC4 key
//! - `req1 = SHA1("req1" || S)` — used as hash marker
//! - `req2 = SHA1("req2" || infoHash)` — used for info_hash verification
//! - `req3 = SHA1("req3" || S)` — XORed with req2
//! - VC = 8 zero bytes encrypted with RC4 (NOT a SHA1 hash)
//! - Both encryptor and decryptor discard first 1024 bytes of RC4 keystream

use sha1::{Digest, Sha1};

use super::mse_dh::{INFO_HASH_LENGTH, KEY_LENGTH, SHA1_LENGTH, VC_LENGTH};

const KEYSTREAM_DISCARD: usize = 1024;

/// MSE crypto negotiation methods (4-byte big-endian bitmask).
///
/// Bit values match C++ aria2 `CRYPTO_PLAIN_TEXT = 0x01`, `CRYPTO_ARC4 = 0x02`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MseCryptoMethod {
    /// No encryption (plaintext after MSE negotiation).
    Plain = 0x00000001,
    /// RC4 stream cipher.
    Rc4 = 0x00000002,
}

impl MseCryptoMethod {
    /// Parse from the least-significant byte of the 4-byte crypto bitmask.
    pub fn from_u32(v: u32) -> Self {
        if v & 0x02 != 0 {
            Self::Rc4
        } else if v & 0x01 != 0 {
            Self::Plain
        } else {
            Self::Plain
        }
    }

    /// Convert to the 4-byte big-endian bitmask value.
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// Whether this method provides actual encryption.
    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Rc4)
    }
}

/// ARC4 (Alleged RC4) stream cipher implementation for MSE.
/// Symmetric: encryption and decryption are the same operation.
#[derive(Debug, Clone)]
pub struct Arc4Cipher {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Arc4Cipher {
    /// Create a new ARC4 cipher instance with the given key.
    pub fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        #[allow(clippy::needless_range_loop)]
        for i in 0..256 {
            s[i] = i as u8;
        }

        let mut j: usize = 0;
        #[allow(clippy::needless_range_loop)]
        for i in 0..256 {
            let si = s[i] as usize;
            j = (j + si + key[i % key.len()] as usize) % 256;
            s.swap(i, j);
        }

        Arc4Cipher { s, i: 0, j: 0 }
    }

    /// Encrypt/decrypt data in-place (ARC4 is symmetric).
    pub fn encrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k =
                self.s[(self.s[self.i as usize] as usize + self.s[self.j as usize] as usize) % 256];
            *byte ^= k;
        }
    }

    /// Decrypt data in-place (same as encrypt for ARC4).
    pub fn decrypt(&mut self, data: &mut [u8]) {
        self.encrypt(data);
    }
}

/// Derived keys for the MSE handshake, matching C++ `MSEHandshake::initCipher()`.
///
/// All keys are full 20-byte SHA-1 digests, matching C++ which uses the
/// complete `localCipherKey[20]` / `peerCipherKey[20]` as RC4 keys.
#[derive(Debug, Clone)]
pub struct MseDerivedKeys {
    /// `SHA1("keyA" || S || infoHash)` — encryptor key for initiator,
    /// decryptor key for receiver.
    pub key_a: [u8; SHA1_LENGTH],
    /// `SHA1("keyB" || S || infoHash)` — decryptor key for initiator,
    /// encryptor key for receiver.
    pub key_b: [u8; SHA1_LENGTH],
    /// `SHA1("req1" || S)` — hash marker sent in step 3.
    pub req1: [u8; SHA1_LENGTH],
    /// `SHA1("req2" || infoHash)` — used to verify info_hash.
    pub req2: [u8; SHA1_LENGTH],
    /// `SHA1("req3" || S)` — XORed with req2 to conceal info_hash.
    pub req3: [u8; SHA1_LENGTH],
}

impl MseDerivedKeys {
    /// Derive all MSE keys from the DH shared secret and info_hash.
    ///
    /// Matches C++ `MSEHandshake::initCipher()` and the hash functions:
    /// - `keyA = SHA1("keyA" || S || infoHash)` (20 bytes)
    /// - `keyB = SHA1("keyB" || S || infoHash)` (20 bytes)
    /// - `req1 = SHA1("req1" || S)` (20 bytes)
    /// - `req2 = SHA1("req2" || infoHash)` (20 bytes)
    /// - `req3 = SHA1("req3" || S)` (20 bytes)
    ///
    /// Note: VC is NOT derived here — it is 8 zero bytes encrypted with
    /// the appropriate RC4 cipher at handshake time.
    pub fn derive(shared_secret: &[u8; KEY_LENGTH], info_hash: &[u8; INFO_HASH_LENGTH]) -> Self {
        // keyA = SHA1("keyA" || S || infoHash)
        let key_a = {
            let mut input = Vec::with_capacity(4 + KEY_LENGTH + INFO_HASH_LENGTH);
            input.extend_from_slice(b"keyA");
            input.extend_from_slice(shared_secret);
            input.extend_from_slice(info_hash);
            sha1_digest_array(&input)
        };

        // keyB = SHA1("keyB" || S || infoHash)
        let key_b = {
            let mut input = Vec::with_capacity(4 + KEY_LENGTH + INFO_HASH_LENGTH);
            input.extend_from_slice(b"keyB");
            input.extend_from_slice(shared_secret);
            input.extend_from_slice(info_hash);
            sha1_digest_array(&input)
        };

        // req1 = SHA1("req1" || S)
        let req1 = {
            let mut input = Vec::with_capacity(4 + KEY_LENGTH);
            input.extend_from_slice(b"req1");
            input.extend_from_slice(shared_secret);
            sha1_digest_array(&input)
        };

        // req2 = SHA1("req2" || infoHash)
        let req2 = {
            let mut input = Vec::with_capacity(4 + INFO_HASH_LENGTH);
            input.extend_from_slice(b"req2");
            input.extend_from_slice(info_hash);
            sha1_digest_array(&input)
        };

        // req3 = SHA1("req3" || S)
        let req3 = {
            let mut input = Vec::with_capacity(4 + KEY_LENGTH);
            input.extend_from_slice(b"req3");
            input.extend_from_slice(shared_secret);
            sha1_digest_array(&input)
        };

        MseDerivedKeys {
            key_a,
            key_b,
            req1,
            req2,
            req3,
        }
    }

    /// Compute `req2 XOR req3` — the 20-byte hash sent in step 3
    /// that allows the receiver to verify the info_hash without
    /// revealing it in plaintext.
    pub fn req2_xor_req3(&self) -> [u8; SHA1_LENGTH] {
        let mut result = [0u8; SHA1_LENGTH];
        for i in 0..SHA1_LENGTH {
            result[i] = self.req2[i] ^ self.req3[i];
        }
        result
    }
}

/// Compute SHA-1 digest and return as a fixed 20-byte array.
fn sha1_digest_array(data: &[u8]) -> [u8; SHA1_LENGTH] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut arr = [0u8; SHA1_LENGTH];
    arr.copy_from_slice(&result);
    arr
}

/// RC4 cipher state with 1024-byte keystream discard (MSE spec section 5.2).
#[derive(Debug)]
pub struct Rc4State {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4State {
    fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        #[allow(clippy::needless_range_loop)]
        for i in 0..256 {
            s[i] = i as u8;
        }

        let mut j: usize = 0;
        #[allow(clippy::needless_range_loop)]
        for i in 0..256 {
            let si = s[i] as usize;
            j = (j + si + key[i % key.len()] as usize) % 256;
            s.swap(i, j);
        }

        Rc4State { s, i: 0, j: 0 }
    }

    /// Process (encrypt/decrypt) data in-place.
    pub fn process(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k =
                self.s[(self.s[self.i as usize] as usize + self.s[self.j as usize] as usize) % 256];
            *byte ^= k;
        }
    }
}

/// Initialize an RC4 cipher with the given key and discard 1024 bytes
/// of keystream, matching C++ `ARC4Encryptor::init()` + discard.
pub fn init_rc4(key: &[u8]) -> Rc4State {
    let mut rc4 = Rc4State::new(key);
    let mut discard = [0u8; KEYSTREAM_DISCARD];
    rc4.process(&mut discard);
    rc4
}

/// Compute the VC (Verification Constant): 8 zero bytes encrypted
/// with the given RC4 cipher. This matches C++ `initCipher()` which
/// pre-computes `initiatorVCMarker_` by encrypting zeros.
///
/// IMPORTANT: The cipher must already have 1024 bytes discarded.
pub fn compute_vc(cipher: &mut Rc4State) -> [u8; VC_LENGTH] {
    let mut vc = [0u8; VC_LENGTH];
    cipher.process(&mut vc);
    vc
}

/// Ongoing MSE crypto state used after the handshake completes.
/// Provides encrypt/decrypt for the BT message stream.
#[derive(Debug)]
pub struct MseCryptoState {
    send_cipher: Option<Rc4State>,
    recv_cipher: Option<Rc4State>,
    method: MseCryptoMethod,
}

impl MseCryptoState {
    /// Create a plaintext (no encryption) state.
    pub fn new_plain() -> Self {
        MseCryptoState {
            send_cipher: None,
            recv_cipher: None,
            method: MseCryptoMethod::Plain,
        }
    }

    /// Create an encrypted state with the given derived keys.
    ///
    /// Matches C++ `initCipher()`:
    /// - Initiator: encryptor = keyA, decryptor = keyB
    /// - Receiver: encryptor = keyB, decryptor = keyA
    pub fn new_encrypted(keys: &MseDerivedKeys, initiator: bool) -> Self {
        if initiator {
            MseCryptoState {
                send_cipher: Some(init_rc4(&keys.key_a)),
                recv_cipher: Some(init_rc4(&keys.key_b)),
                method: MseCryptoMethod::Rc4,
            }
        } else {
            MseCryptoState {
                send_cipher: Some(init_rc4(&keys.key_b)),
                recv_cipher: Some(init_rc4(&keys.key_a)),
                method: MseCryptoMethod::Rc4,
            }
        }
    }

    /// Create an encrypted state from raw 20-byte RC4 keys.
    /// This allows the handshake code to pass the exact keys used
    /// during negotiation (which may differ from the derived keys
    /// if a different crypto method was selected).
    pub fn from_raw_keys(send_key: &[u8; SHA1_LENGTH], recv_key: &[u8; SHA1_LENGTH], method: MseCryptoMethod) -> Self {
        match method {
            MseCryptoMethod::Rc4 => MseCryptoState {
                send_cipher: Some(init_rc4(send_key)),
                recv_cipher: Some(init_rc4(recv_key)),
                method: MseCryptoMethod::Rc4,
            },
            MseCryptoMethod::Plain => MseCryptoState::new_plain(),
        }
    }

    /// Encrypt data in-place (send direction).
    pub fn encrypt(&mut self, data: &mut [u8]) {
        if let Some(ref mut cipher) = self.send_cipher {
            cipher.process(data);
        }
    }

    /// Decrypt data in-place (receive direction).
    pub fn decrypt(&mut self, data: &mut [u8]) {
        if let Some(ref mut cipher) = self.recv_cipher {
            cipher.process(data);
        }
    }

    /// Whether encryption is active.
    pub fn is_encrypted(&self) -> bool {
        self.send_cipher.is_some()
    }

    /// The negotiated crypto method.
    pub fn method(&self) -> MseCryptoMethod {
        self.method
    }
}

impl Default for MseCryptoState {
    fn default() -> Self {
        Self::new_plain()
    }
}

impl Clone for MseCryptoState {
    fn clone(&self) -> Self {
        // RC4 state cannot be meaningfully cloned (stream position lost).
        // Return a plain state; callers who need to replicate must use
        // the same key material to create a new instance.
        MseCryptoState::new_plain()
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(encrypted, original.to_vec(), "decrypted should match original");
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
        for i in 0..SHA1_LENGTH {
            expected[i] = keys.req2[i] ^ keys.req3[i];
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
        assert_ne!(vc, [0u8; VC_LENGTH], "VC should not be all zeros after RC4 encryption");

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
}
