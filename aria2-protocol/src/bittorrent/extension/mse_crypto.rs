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
        for (result, (&req2, &req3)) in result
            .iter_mut()
            .zip(self.req2.iter().zip(self.req3.iter()))
        {
            *result = req2 ^ req3;
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
    pub fn from_raw_keys(
        send_key: &[u8; SHA1_LENGTH],
        recv_key: &[u8; SHA1_LENGTH],
        method: MseCryptoMethod,
    ) -> Self {
        match method {
            MseCryptoMethod::Rc4 => MseCryptoState {
                send_cipher: Some(init_rc4(send_key)),
                recv_cipher: Some(init_rc4(recv_key)),
                method: MseCryptoMethod::Rc4,
            },
            MseCryptoMethod::Plain => MseCryptoState::new_plain(),
        }
    }

    /// Continue an MSE stream with the cipher positions reached by the
    /// handshake. Reinitializing from raw keys here would replay the first
    /// keystream bytes and corrupt the post-MSE BitTorrent handshake.
    pub(crate) fn from_rc4_states(
        send_cipher: Rc4State,
        recv_cipher: Rc4State,
        method: MseCryptoMethod,
    ) -> Self {
        Self {
            send_cipher: Some(send_cipher),
            recv_cipher: Some(recv_cipher),
            method,
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
mod tests;
