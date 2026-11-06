//! MSE (Message Stream Encryption) handshake state machine.
//!
//! The wire exchange matches aria2's four-step MSE protocol. Initiator and
//! responder operations live in private submodules; this module owns shared
//! state, DH setup, finalization, and marker/hash helpers.

mod initiator;
mod receiver;
#[cfg(test)]
#[path = "mse_handshake/tests.rs"]
mod tests;

use super::mse_crypto::{
    MseCryptoMethod, MseCryptoState, MseDerivedKeys, Rc4State, compute_vc, init_rc4,
};
use super::mse_dh::{
    CRYPTO_BITFIELD_LENGTH, INFO_HASH_LENGTH, KEY_LENGTH, MAX_PAD_LENGTH, MseDhKeyExchange,
    SHA1_LENGTH, VC_LENGTH,
};
use rand::Rng;

/// Synchronization limit for the initiator: 616 bytes max before finding VC marker.
const INITIATOR_SYNC_LIMIT: usize = 616;

/// Synchronization limit for the receiver: 628 bytes max before finding req1 marker.
const RECEIVER_SYNC_LIMIT: usize = 628;

/// MSE initial data carries at most the 68-byte BitTorrent handshake.
const MAX_INITIAL_DATA_LENGTH: usize = 68;

/// Public wire limits used by the asynchronous incoming-peer adapter.
pub const MSE_PUBLIC_KEY_LENGTH: usize = KEY_LENGTH;
pub const MSE_MAX_BUFFER_LENGTH: usize = 636;
/// Maximum absolute size of the incoming step-2 wire payload.
pub const MSE_MAX_INCOMING_HANDSHAKE_LENGTH: usize = MAX_PAD_LENGTH
    + SHA1_LENGTH
    + SHA1_LENGTH
    + VC_LENGTH
    + CRYPTO_BITFIELD_LENGTH
    + 2
    + MAX_PAD_LENGTH
    + 2
    + MAX_INITIAL_DATA_LENGTH;

/// Handshake phase tracking.
#[derive(Debug, Clone, PartialEq)]
pub enum MseHandshakePhase {
    /// Initial state; DH keys generated but nothing sent/received.
    Idle,
    /// Public key sent; waiting for remote public key.
    PublicKeySent,
    /// Remote public key received; shared secret computed.
    PublicKeyReceived,
    /// Initiator step 2 sent (req1 + req2^req3 + encrypted payload).
    InitiatorStep2Sent,
    /// Handshake completed successfully.
    Completed(MseCryptoMethod),
    /// Handshake failed.
    Failed(String),
}

/// MSE handshake state shared by the initiator and responder state machines.
pub struct MseHandshake {
    phase: MseHandshakePhase,
    dh: MseDhKeyExchange,
    initiator: bool,
    info_hash: [u8; INFO_HASH_LENGTH],
    shared_secret: Option<[u8; KEY_LENGTH]>,
    keys: Option<MseDerivedKeys>,
    negotiated_method: MseCryptoMethod,
    force_encryption: bool,
    prefer_encryption: bool,
    initiator_vc_marker: Option<[u8; VC_LENGTH]>,
    initiator_encryptor: Option<Rc4State>,
    initiator_decryptor: Option<Rc4State>,
    receiver_encryptor: Option<Rc4State>,
    receiver_decryptor: Option<Rc4State>,
    receiver_initial_payload: Vec<u8>,
}

impl MseHandshake {
    /// Create a new initiator handshake.
    pub fn new_initiator(info_hash: [u8; INFO_HASH_LENGTH]) -> Self {
        Self::new(info_hash, true)
    }

    /// Create a new responder handshake.
    pub fn new_responder(info_hash: [u8; INFO_HASH_LENGTH]) -> Self {
        Self::new(info_hash, false)
    }

    fn new(info_hash: [u8; INFO_HASH_LENGTH], initiator: bool) -> Self {
        Self {
            phase: MseHandshakePhase::Idle,
            dh: MseDhKeyExchange::new(),
            initiator,
            info_hash,
            shared_secret: None,
            keys: None,
            negotiated_method: MseCryptoMethod::Plain,
            force_encryption: false,
            prefer_encryption: true,
            initiator_vc_marker: None,
            initiator_encryptor: None,
            initiator_decryptor: None,
            receiver_encryptor: None,
            receiver_decryptor: None,
            receiver_initial_payload: Vec::new(),
        }
    }

    /// Set encryption preferences.
    pub fn set_crypto_preferences(&mut self, force_encryption: bool, prefer_encryption: bool) {
        self.force_encryption = force_encryption;
        self.prefer_encryption = prefer_encryption;
    }

    /// Get the current handshake phase.
    pub fn phase(&self) -> &MseHandshakePhase {
        &self.phase
    }

    /// Get the info_hash.
    pub fn info_hash(&self) -> &[u8; INFO_HASH_LENGTH] {
        &self.info_hash
    }

    /// Get the negotiated crypto method (valid after completion).
    pub fn negotiated_method(&self) -> MseCryptoMethod {
        self.negotiated_method
    }

    /// Build the DH public-key payload with MSE padding.
    pub fn build_step1(&self) -> Vec<u8> {
        let mut rng = rand::thread_rng();
        let pad_length: usize = rng.gen_range(0..=MAX_PAD_LENGTH);
        let mut buffer = Vec::with_capacity(KEY_LENGTH + pad_length);
        buffer.extend_from_slice(&self.dh.generate_public_key());
        let mut padding = vec![0u8; pad_length];
        rng.fill(&mut padding[..]);
        buffer.extend_from_slice(&padding);
        buffer
    }

    /// Process the received public-key payload and initialize shared MSE keys.
    pub fn receive_step1(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < KEY_LENGTH {
            return Err(format!(
                "Step1 data too short: got {} bytes, need at least {}",
                data.len(),
                KEY_LENGTH
            ));
        }

        let remote_public: [u8; KEY_LENGTH] = data[..KEY_LENGTH]
            .try_into()
            .expect("public-key prefix has the fixed key length");
        let shared = self.dh.compute_shared_secret(&remote_public);
        self.shared_secret = Some(shared);
        let keys = MseDerivedKeys::derive(&shared, &self.info_hash);

        if self.initiator {
            self.initiator_encryptor = Some(init_rc4(&keys.key_a));
            let mut marker_cipher = init_rc4(&keys.key_b);
            self.initiator_vc_marker = Some(compute_vc(&mut marker_cipher));
            self.initiator_decryptor = Some(init_rc4(&keys.key_b));
        }

        self.keys = Some(keys);
        self.phase = MseHandshakePhase::PublicKeyReceived;
        Ok(())
    }

    /// Finalize the handshake and return the ongoing crypto state.
    pub fn finalize(mut self) -> Result<MseCryptoState, String> {
        match self.phase {
            MseHandshakePhase::Completed(MseCryptoMethod::Plain) => Ok(MseCryptoState::new_plain()),
            MseHandshakePhase::Completed(MseCryptoMethod::Rc4) if self.initiator => {
                let send = self
                    .initiator_encryptor
                    .take()
                    .ok_or("Initiator encryptor not initialized")?;
                let recv = self
                    .initiator_decryptor
                    .take()
                    .ok_or("Initiator decryptor not initialized")?;
                Ok(MseCryptoState::from_rc4_states(
                    send,
                    recv,
                    MseCryptoMethod::Rc4,
                ))
            }
            MseHandshakePhase::Completed(MseCryptoMethod::Rc4) => {
                let send = self
                    .receiver_encryptor
                    .take()
                    .ok_or("Receiver encryptor not initialized")?;
                let recv = self
                    .receiver_decryptor
                    .take()
                    .ok_or("Receiver decryptor not initialized")?;
                Ok(MseCryptoState::from_rc4_states(
                    send,
                    recv,
                    MseCryptoMethod::Rc4,
                ))
            }
            MseHandshakePhase::Failed(error) => Err(error),
            _ => Err(format!("Handshake not completed: {:?}", self.phase)),
        }
    }

    /// Determine whether MSE should be negotiated based on reserved bytes.
    pub fn should_negotiate(local_supports_mse: bool, remote_reserved: &[u8]) -> bool {
        local_supports_mse && remote_reserved.len() >= 8 && (remote_reserved[7] & 0x01) != 0
    }

    /// Identify whether incoming data is a legacy BT handshake or encrypted.
    pub fn identify_handshake_type(data: &[u8]) -> HandshakeType {
        if data.len() < 20 {
            return HandshakeType::NotYet;
        }
        if data[0] == 19 && &data[1..20] == b"BitTorrent protocol" {
            HandshakeType::Legacy
        } else {
            HandshakeType::Encrypted
        }
    }
}

/// Result of identifying the handshake type from incoming data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeType {
    /// Not enough data yet.
    NotYet,
    /// Standard BT handshake detected.
    Legacy,
    /// Encrypted (MSE) handshake detected.
    Encrypted,
}

fn find_req1_marker(
    data: &[u8],
    req1_hash: &[u8; SHA1_LENGTH],
    sync_limit: usize,
) -> Result<usize, String> {
    if data.len() < SHA1_LENGTH {
        return Err("Data too short for req1 marker search".to_string());
    }
    for i in 0..=(data.len().saturating_sub(SHA1_LENGTH)) {
        if data[i..].starts_with(req1_hash) {
            return Ok(i);
        }
        if i + SHA1_LENGTH > sync_limit {
            return Err("Failed to find req1 hash marker within sync limit".to_string());
        }
    }
    Err("Failed to find req1 hash marker".to_string())
}

fn find_marker_if_present(data: &[u8], marker: &[u8]) -> Option<usize> {
    data.windows(marker.len())
        .position(|window| window == marker)
}

fn find_vc_marker(
    data: &[u8],
    vc_marker: &[u8; VC_LENGTH],
    sync_limit: usize,
) -> Result<usize, String> {
    if data.len() < VC_LENGTH {
        return Err("Data too short for VC marker search".to_string());
    }
    for i in 0..=(data.len().saturating_sub(VC_LENGTH)) {
        if data[i..].starts_with(vc_marker) {
            return Ok(i);
        }
        if i + VC_LENGTH > sync_limit - KEY_LENGTH {
            return Err("Failed to find VC marker within sync limit".to_string());
        }
    }
    Err("Failed to find VC marker".to_string())
}

fn verify_req2_xor_req3(
    req2_xor_req3: &[u8],
    known_info_hashes: &[[u8; INFO_HASH_LENGTH]],
    req3: &[u8; SHA1_LENGTH],
) -> Option<[u8; INFO_HASH_LENGTH]> {
    for info_hash in known_info_hashes {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(b"req2");
        hasher.update(info_hash);
        let digest = hasher.finalize();
        let mut expected = [0u8; SHA1_LENGTH];
        for (index, (req2_byte, req3_byte)) in digest.iter().zip(req3).enumerate() {
            expected[index] = req2_byte ^ req3_byte;
        }
        if req2_xor_req3 == expected {
            return Some(*info_hash);
        }
    }
    None
}
