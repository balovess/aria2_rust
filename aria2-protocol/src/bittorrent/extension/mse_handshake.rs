use rand::Rng;
use super::mse_dh::DhKeyPair;
use super::mse_crypto::{MseCryptoMethod, MseCryptoState, MseDerivedKeys};

const PAD_A_MIN: usize = 0;
const PAD_A_MAX: usize = 512;
const PAD_B_MIN: usize = 0;
const PAD_B_MAX: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub enum MseHandshakePhase {
    Idle,
    KeysExchanged,
    LocalCryptoProvided,
    Completed(MseCryptoMethod),
    Failed(String),
}

pub struct MseHandshake {
    phase: MseHandshakePhase,
    dh_keypair: DhKeyPair,
    remote_public: Option<Vec<u8>>,
    ya_pad: [u8; 16],
    yb_pad: [u8; 16],
    initiator: bool,
    keys: Option<MseDerivedKeys>,
    selected_method: MseCryptoMethod,
}

impl MseHandshake {
    pub fn new_initiator() -> Self {
        let mut rng = rand::thread_rng();
        let mut ya_pad = [0u8; 16];
        rng.fill(&mut ya_pad);

        MseHandshake {
            phase: MseHandshakePhase::Idle,
            dh_keypair: DhKeyPair::generate(),
            remote_public: None,
            ya_pad,
            yb_pad: [0u8; 16],
            initiator: true,
            keys: None,
            selected_method: MseCryptoMethod::Plain,
        }
    }

    pub fn new_responder() -> Self {
        let mut rng = rand::thread_rng();
        let mut yb_pad = [0u8; 16];
        rng.fill(&mut yb_pad);

        MseHandshake {
            phase: MseHandshakePhase::Idle,
            dh_keypair: DhKeyPair::generate(),
            remote_public: None,
            ya_pad: [0u8; 16],
            yb_pad,
            initiator: false,
            keys: None,
            selected_method: MseCryptoMethod::Plain,
        }
    }

    pub fn build_step1(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(16 + self.dh_keypair.public.len());
        if self.initiator {
            result.extend_from_slice(&self.ya_pad);
        } else {
            result.extend_from_slice(&self.yb_pad);
        }
        result.extend_from_slice(&self.dh_keypair.public);
        result
    }

    pub fn receive_step1(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < 16 {
            return Err("Step1 data too short (< 16 bytes)".to_string());
        }
        let (pad, pubkey) = data.split_at(16);
        if self.initiator {
            self.yb_pad.copy_from_slice(pad);
        } else {
            self.ya_pad.copy_from_slice(pad);
        }
        self.remote_public = Some(pubkey.to_vec());
        self.phase = MseHandshakePhase::KeysExchanged;
        Ok(())
    }

    pub fn build_step2(&mut self) -> Result<Vec<u8>, String> {
        if !matches!(self.phase, MseHandshakePhase::KeysExchanged) {
            return Err("Cannot build Step2: keys not yet exchanged".to_string());
        }
        let remote_pub = self.remote_public.as_ref()
            .ok_or("No remote public key")?;
        let shared_secret = self.dh_keypair.compute_shared_secret(remote_pub);
        let keys = MseDerivedKeys::derive(&shared_secret);

        let mut rng = rand::thread_rng();
        let pad_d_len = rng.gen_range(PAD_A_MIN..=PAD_A_MAX);
        let pad_e_len = rng.gen_range(PAD_B_MIN..=PAD_B_MAX);
        let mut pad_d = vec![0u8; pad_d_len];
        let mut pad_e = vec![0u8; pad_e_len];
        rng.fill(&mut pad_d[..]);
        rng.fill(&mut pad_e[..]);

        let crypto_provide = MseCryptoMethod::Rc4.as_u32().to_be_bytes();

        let vc = if self.initiator { &keys.vc_a } else { &keys.vc_b };

        let mut plain = Vec::with_capacity(8 + 4 + 2 + pad_d_len + 2 + pad_e_len);
        plain.extend_from_slice(vc);
        plain.extend_from_slice(&crypto_provide);
        plain.extend_from_slice(&(pad_d_len as u16).to_be_bytes());
        plain.extend_from_slice(&pad_d);
        plain.extend_from_slice(&(pad_e_len as u16).to_be_bytes());
        plain.extend_from_slice(&pad_e);

        let enc_key = if self.initiator { &keys.enc_key_a } else { &keys.enc_key_b };
        let mut crypto = super::mse_crypto::init_rc4(enc_key);
        crypto.process(&mut plain);

        self.phase = MseHandshakePhase::LocalCryptoProvided;
        Ok(plain)
    }

    pub fn receive_step2(&mut self, encrypted_data: &[u8]) -> Result<MseCryptoMethod, String> {
        let remote_pub = self.remote_public.as_ref()
            .ok_or("No remote public key for step2")?;
        let shared_secret = self.dh_keypair.compute_shared_secret(remote_pub);
        let keys = MseDerivedKeys::derive(&shared_secret);

        let recv_enc_key = if self.initiator { &keys.enc_key_b } else { &keys.enc_key_a };
        let expected_vc = if self.initiator { &keys.vc_b } else { &keys.vc_a };

        let mut dec = encrypted_data.to_vec();
        let mut crypto = super::mse_crypto::init_rc4(recv_enc_key);
        crypto.process(&mut dec);

        if dec.len() < 12 {
            return Err("Step2 decrypted data too short".to_string());
        }

        let received_vc = &dec[0..8];
        if received_vc != *expected_vc {
            return Err(format!("VC verification failed: expected {:?}, got {:?}", expected_vc, received_vc));
        }

        let crypto_provide = u32::from_be_bytes([dec[8], dec[9], dec[10], dec[11]]);
        let method = MseCryptoMethod::from_u32(crypto_provide);

        match method {
            MseCryptoMethod::Plain => {
                self.selected_method = MseCryptoMethod::Plain;
                self.keys = Some(keys);
                self.phase = MseHandshakePhase::Completed(MseCryptoMethod::Plain);
                Ok(MseCryptoMethod::Plain)
            }
            MseCryptoMethod::Rc4 | MseCryptoMethod::Aes128Cfb => {
                self.selected_method = method;
                self.keys = Some(keys);
                self.phase = MseHandshakePhase::Completed(method);
                Ok(method)
            }
        }
    }

    pub fn finalize(self) -> Result<MseCryptoState, String> {
        match self.phase {
            MseHandshakePhase::Completed(method) => {
                let keys = self.keys.ok_or("Keys not derived")?;
                match method {
                    MseCryptoMethod::Plain => Ok(MseCryptoState::new_plain()),
                    _ => Ok(MseCryptoState::new_encrypted(&keys, self.initiator)),
                }
            }
            MseHandshakePhase::Failed(e) => Err(e),
            _ => Err(format!("Handshake not completed: {:?}", self.phase)),
        }
    }

    pub fn should_negotiate(local_supports_mse: bool, remote_reserved: &[u8]) -> bool {
        local_supports_mse && !remote_reserved.is_empty() && (remote_reserved[0] & 0x01) != 0
    }

    pub fn phase(&self) -> &MseHandshakePhase {
        &self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initiator_responder_full_handshake() {
        let mut initiator = MseHandshake::new_initiator();
        let mut responder = MseHandshake::new_responder();

        let step1_i = initiator.build_step1();
        assert!(step1_i.len() >= 16, "Step1 must include padding");

        responder.receive_step1(&step1_i).expect("Responder receive step1");

        let step1_r = responder.build_step1();
        initiator.receive_step1(&step1_r).expect("Initiator receive step1");

        let step2_i = initiator.build_step2().expect("Initiator build step2");
        assert!(!step2_i.is_empty(), "Step2 should have data");

        let step2_r = responder.build_step2().expect("Responder build step2");

        let method_r = responder.receive_step2(&step2_i).expect("Responder receive step2");
        assert_eq!(method_r, MseCryptoMethod::Rc4);

        let method_i = initiator.receive_step2(&step2_r).expect("Initiator receive step2");

        let crypto_i = initiator.finalize().expect("Initiator finalize");
        let crypto_r = responder.finalize().expect("Responder finalize");

        assert!(crypto_i.is_encrypted());
        assert!(crypto_r.is_encrypted());
    }

    #[test]
    fn test_step1_format() {
        let h = MseHandshake::new_initiator();
        let s1 = h.build_step1();
        assert!(s1.len() >= 144, "Step1 >= 16 (pad) + 128 (pub key min)");
    }

    #[test]
    fn test_should_negotiate_combinations() {
        assert!(!MseHandshake::should_negotiate(true, &[0x00]));
        assert!(MseHandshake::should_negotiate(true, &[0x01]));
        assert!(!MseHandshake::should_negotiate(false, &[0x01]));
        assert!(MseHandshake::should_negotiate(true, &[0xFF]));
        assert!(!MseHandshake::should_negotiate(true, &[]));
    }

    #[test]
    fn test_receive_step1_too_short() {
        let mut h = MseHandshake::new_responder();
        assert!(h.receive_step1(&[0u8; 8]).is_err());
    }

    #[test]
    fn test_build_step2_before_keys_exchanged() {
        let mut h = MseHandshake::new_initiator();
        assert!(h.build_step2().is_err());
    }

    #[test]
    fn test_finalize_before_completed() {
        let h = MseHandshake::new_initiator();
        assert!(h.finalize().is_err());
    }

    #[test]
    fn test_different_instances_different_keys() {
        let h1 = MseHandshake::new_initiator();
        let h2 = MseHandshake::new_initiator();
        assert_ne!(h1.dh_keypair.public, h2.dh_keypair.public);
    }
}
