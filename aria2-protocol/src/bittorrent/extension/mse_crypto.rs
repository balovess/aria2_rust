use sha1::{Digest, Sha1};

const KEYSTREAM_DISCARD: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MseCryptoMethod {
    Plain = 0x00000000,
    Rc4 = 0x00000001,
    Aes128Cfb = 0x00000002,
}

impl MseCryptoMethod {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0x00000001 => Self::Rc4,
            0x00000002 => Self::Aes128Cfb,
            _ => Self::Plain,
        }
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

#[derive(Debug, Clone)]
pub struct MseDerivedKeys {
    pub skey: [u8; 20],
    pub enc_key_a: [u8; 16],
    pub enc_key_b: [u8; 16],
    pub enc_key2_a: [u8; 16],
    pub enc_key2_b: [u8; 16],
    pub vc_a: [u8; 8],
    pub vc_b: [u8; 8],
}

impl MseDerivedKeys {
    pub fn derive(shared_secret: &[u8]) -> Self {
        let skey_arr = sha1_digest(shared_secret);

        let mut key_a_input = shared_secret.to_vec();
        key_a_input.extend_from_slice(b"keyA");
        let enc_key_a_full = sha1_digest(&key_a_input);

        let mut key_b_input = shared_secret.to_vec();
        key_b_input.extend_from_slice(b"keyB");
        let enc_key_b_full = sha1_digest(&key_b_input);

        let mut key2a_input = enc_key_a_full.clone();
        key2a_input.extend_from_slice(b"request1");
        let enc_key2_a_full = sha1_digest(&key2a_input);

        let mut key2b_input = enc_key_b_full.clone();
        key2b_input.extend_from_slice(b"response1");
        let enc_key2_b_full = sha1_digest(&key2b_input);

        let mut vc_a_input = shared_secret.to_vec();
        vc_a_input.extend_from_slice(b"req1");
        let vc_a_full = sha1_digest(&vc_a_input);

        let mut vc_b_input = shared_secret.to_vec();
        vc_b_input.extend_from_slice(b"req2");
        let vc_b_full = sha1_digest(&vc_b_input);

        let mut skey = [0u8; 20];
        let mut enc_key_a = [0u8; 16];
        let mut enc_key_b = [0u8; 16];
        let mut enc_key2_a = [0u8; 16];
        let mut enc_key2_b = [0u8; 16];
        let mut vc_a = [0u8; 8];
        let mut vc_b = [0u8; 8];

        skey.copy_from_slice(&skey_arr[..20]);
        enc_key_a.copy_from_slice(&enc_key_a_full[..16]);
        enc_key_b.copy_from_slice(&enc_key_b_full[..16]);
        enc_key2_a.copy_from_slice(&enc_key2_a_full[..16]);
        enc_key2_b.copy_from_slice(&enc_key2_b_full[..16]);
        vc_a.copy_from_slice(&vc_a_full[..8]);
        vc_b.copy_from_slice(&vc_b_full[..8]);

        MseDerivedKeys {
            skey,
            enc_key_a,
            enc_key_b,
            enc_key2_a,
            enc_key2_b,
            vc_a,
            vc_b,
        }
    }
}

fn sha1_digest(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

pub struct Rc4State {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4State {
    fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for i in 0..256 {
            s[i] = i as u8;
        }

        let mut j: usize = 0;
        for i in 0..256 {
            j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
            s.swap(i, j);
        }

        Rc4State { s, i: 0, j: 0 }
    }

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

pub struct MseCryptoState {
    send_cipher: Option<Rc4State>,
    recv_cipher: Option<Rc4State>,
}

impl MseCryptoState {
    pub fn new_plain() -> Self {
        MseCryptoState {
            send_cipher: None,
            recv_cipher: None,
        }
    }

    pub fn new_encrypted(keys: &MseDerivedKeys, initiator: bool) -> Self {
        if initiator {
            MseCryptoState {
                send_cipher: Some(init_rc4(&keys.enc_key2_a)),
                recv_cipher: Some(init_rc4(&keys.enc_key2_b)),
            }
        } else {
            MseCryptoState {
                send_cipher: Some(init_rc4(&keys.enc_key2_b)),
                recv_cipher: Some(init_rc4(&keys.enc_key2_a)),
            }
        }
    }

    pub fn encrypt(&mut self, data: &mut [u8]) {
        if let Some(ref mut cipher) = self.send_cipher {
            cipher.process(data);
        }
    }

    pub fn decrypt(&mut self, data: &mut [u8]) {
        if let Some(ref mut cipher) = self.recv_cipher {
            cipher.process(data);
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.send_cipher.is_some()
    }
}

pub fn init_rc4(key: &[u8]) -> Rc4State {
    let mut rc4 = Rc4State::new(key);
    let mut discard = [0u8; KEYSTREAM_DISCARD];
    rc4.process(&mut discard);
    rc4
}

impl Default for MseCryptoState {
    fn default() -> Self {
        Self::new_plain()
    }
}

impl Clone for MseCryptoState {
    fn clone(&self) -> Self {
        MseCryptoState {
            send_cipher: None,
            recv_cipher: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_keys_deterministic() {
        let secret = b"test_shared_secret_12345";
        let keys1 = MseDerivedKeys::derive(secret);
        let keys2 = MseDerivedKeys::derive(secret);

        assert_eq!(keys1.skey, keys2.skey);
        assert_eq!(keys1.enc_key_a, keys2.enc_key_a);
        assert_eq!(keys1.vc_a, keys2.vc_a);
    }

    #[test]
    fn test_crypto_method_roundtrip() {
        assert_eq!(MseCryptoMethod::from_u32(0), MseCryptoMethod::Plain);
        assert_eq!(MseCryptoMethod::from_u32(1), MseCryptoMethod::Rc4);
        assert_eq!(MseCryptoMethod::from_u32(2), MseCryptoMethod::Aes128Cfb);
        assert_eq!(MseCryptoMethod::from_u32(999), MseCryptoMethod::Plain);
        assert_eq!(MseCryptoMethod::Rc4.as_u32(), 1);
    }

    #[test]
    fn test_rc4_encrypt_decrypt_roundtrip() {
        let secret = b"shared_secret_for_encryption_test";
        let keys = MseDerivedKeys::derive(secret);

        let mut crypto_initiator = MseCryptoState::new_encrypted(&keys, true);
        let mut crypto_responder = MseCryptoState::new_encrypted(&keys, false);

        let original = b"Hello, BitTorrent MSE!";
        let mut encrypted = original.to_vec();

        crypto_initiator.encrypt(&mut encrypted);
        assert_ne!(
            encrypted,
            original.to_vec(),
            "encrypted should differ from plaintext"
        );

        crypto_responder.decrypt(&mut encrypted);
        assert_eq!(
            encrypted,
            original.to_vec(),
            "decrypted should match original"
        );
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
    fn test_vc_length_and_nonzero() {
        let keys = MseDerivedKeys::derive(b"any_secret");

        assert_eq!(keys.vc_a.len(), 8);
        assert_eq!(keys.vc_b.len(), 8);
        assert_ne!(keys.vc_a, [0u8; 8], "VC_A should not be all zeros");
        assert_ne!(keys.vc_b, [0u8; 8], "VC_B should not be all zeros");
        assert_ne!(keys.vc_a, keys.vc_b, "VC_A and VC_B should differ");
    }

    #[test]
    fn test_skey_is_sha1_of_secret() {
        let secret = b"my_secret";
        let keys = MseDerivedKeys::derive(secret);

        let expected_skey = sha1_digest(secret);
        assert_eq!(&keys.skey[..], expected_skey.as_slice());
    }

    #[test]
    fn test_multiple_messages_independent() {
        let keys = MseDerivedKeys::derive(b"multi_msg_secret");
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
