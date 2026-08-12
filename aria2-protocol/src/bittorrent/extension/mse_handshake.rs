//! MSE (Message Stream Encryption) handshake state machine.
//!
//! Matches C++ aria2 `MSEHandshake.cc` 4-step wire format:
//!
//! | Step | Direction | Payload |
//! |------|-----------|---------|
//! | 1    | I → R     | `YA \|\| PadA` (96 + 0..512 bytes) |
//! | 2    | R → I     | `YB \|\| PadB` (96 + 0..512 bytes) |
//! | 3    | I → R     | `Hash('req1',S) \|\| [Hash('req2',SKEY) XOR Hash('req3',S)] \|\| E(VC \|\| crypto_provide \|\| len(PadC) \|\| PadC \|\| len(IA) \|\| IA)` |
//! | 4    | R → I     | `E(VC \|\| crypto_select \|\| len(PadD) \|\| PadD)` |
//!
//! Where:
//! - `S` = DH shared secret (96 bytes)
//! - `SKEY` = info_hash (20 bytes)
//! - `VC` = 8 zero bytes, RC4-encrypted
//! - `E(...)` = RC4-encrypted with the initiator's encryptor key (keyA for initiator, keyB for receiver)
//! - `crypto_provide` / `crypto_select` = 4-byte big-endian bitmask

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

/// Public wire limits used by the asynchronous incoming-peer adapter.
pub const MSE_PUBLIC_KEY_LENGTH: usize = KEY_LENGTH;
pub const MSE_MAX_BUFFER_LENGTH: usize = 636;

/// Handshake phase tracking.
#[derive(Debug, Clone, PartialEq)]
pub enum MseHandshakePhase {
    /// Initial state; DH keys generated but nothing sent/received.
    Idle,
    /// Public key sent; waiting for remote public key.
    PublicKeySent,
    /// Remote public key received; shared secret computed.
    PublicKeyReceived,
    /// Initiator step2 sent (req1 + req2^req3 + encrypted payload).
    InitiatorStep2Sent,
    /// Handshake completed successfully.
    Completed(MseCryptoMethod),
    /// Handshake failed.
    Failed(String),
}

/// MSE handshake state machine implementing the C++ 4-step protocol.
pub struct MseHandshake {
    phase: MseHandshakePhase,
    dh: MseDhKeyExchange,
    /// Whether this side initiated the connection.
    initiator: bool,
    /// The info_hash for the torrent being downloaded.
    info_hash: [u8; INFO_HASH_LENGTH],
    /// DH shared secret S (96 bytes, fixed size).
    shared_secret: Option<[u8; KEY_LENGTH]>,
    /// Derived keys (set after shared secret is computed).
    keys: Option<MseDerivedKeys>,
    /// Negotiated crypto method.
    negotiated_method: MseCryptoMethod,
    /// Whether force encryption is required (no plaintext fallback).
    force_encryption: bool,
    /// Minimum crypto level: true = require RC4, false = allow plaintext.
    prefer_encryption: bool,
    /// Initiator's pre-computed VC marker (8 bytes), used for searching.
    /// Matches C++ `initiatorVCMarker_`.
    initiator_vc_marker: Option<[u8; VC_LENGTH]>,
    /// Encryptor RC4 state for the initiator role (keyA-based).
    /// Used during handshake for step 3 encryption and step 4 decryption.
    initiator_encryptor: Option<Rc4State>,
    /// Decryptor RC4 state for the initiator role (keyB-based).
    /// Used during handshake for VC marker computation.
    initiator_decryptor: Option<Rc4State>,
    /// Receiver-side stream states, transferred to the post-handshake peer.
    receiver_encryptor: Option<Rc4State>,
    receiver_decryptor: Option<Rc4State>,
}

impl MseHandshake {
    /// Create a new initiator handshake.
    pub fn new_initiator(info_hash: [u8; INFO_HASH_LENGTH]) -> Self {
        MseHandshake {
            phase: MseHandshakePhase::Idle,
            dh: MseDhKeyExchange::new(),
            initiator: true,
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
        }
    }

    /// Create a new responder handshake.
    pub fn new_responder(info_hash: [u8; INFO_HASH_LENGTH]) -> Self {
        MseHandshake {
            phase: MseHandshakePhase::Idle,
            dh: MseDhKeyExchange::new(),
            initiator: false,
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

    // ── Step 1: Public key exchange ──────────────────────────────────

    /// Build the step 1 payload: `YA || PadA` (initiator) or `YB || PadB` (responder).
    ///
    /// Matches C++ `MSEHandshake::sendPublicKey()`.
    pub fn build_step1(&self) -> Vec<u8> {
        let mut rng = rand::thread_rng();
        let pad_length: usize = rng.gen_range(0..=MAX_PAD_LENGTH);
        let mut buf = Vec::with_capacity(KEY_LENGTH + pad_length);

        // Public key (96 bytes)
        buf.extend_from_slice(&self.dh.generate_public_key());

        // Random padding (0..512 bytes)
        let mut pad = vec![0u8; pad_length];
        rng.fill(&mut pad[..]);
        buf.extend_from_slice(&pad);

        buf
    }

    /// Process the received step 1 payload (remote public key + padding).
    ///
    /// Extracts the remote public key (first KEY_LENGTH bytes) and computes
    /// the shared secret. Matches C++ `MSEHandshake::receivePublicKey()`.
    pub fn receive_step1(&mut self, data: &[u8]) -> Result<(), String> {
        if data.len() < KEY_LENGTH {
            return Err(format!(
                "Step1 data too short: got {} bytes, need at least {}",
                data.len(),
                KEY_LENGTH
            ));
        }

        // The remote public key is the first KEY_LENGTH bytes
        let mut remote_public = [0u8; KEY_LENGTH];
        remote_public.copy_from_slice(&data[..KEY_LENGTH]);

        // Compute shared secret
        let shared = self.dh.compute_shared_secret(&remote_public);
        self.shared_secret = Some(shared);

        // Derive keys using the shared secret and our info_hash
        let keys = MseDerivedKeys::derive(&shared, &self.info_hash);
        self.keys = Some(keys);

        // If initiator, pre-compute the VC marker for step 4 search.
        // Matches C++ initCipher() lines 209-215:
        //   ARC4Encryptor enc;
        //   enc.init(peerCipherKey, sizeof(peerCipherKey));
        //   enc.encrypt(1024 discard);
        //   enc.encrypt(VC_LENGTH, initiatorVCMarker_, VC.data());
        if self.initiator {
            let keys = self.keys.as_ref().expect("keys just set");
            // The initiator's encryptor uses keyA (for sending step 3)
            self.initiator_encryptor = Some(init_rc4(&keys.key_a));
            // The initiator's decryptor uses keyB (for receiving step 4)
            let mut peer_cipher = init_rc4(&keys.key_b);
            // Compute VC marker = RC4(zeros) using peer cipher
            let vc_marker = compute_vc(&mut peer_cipher);
            self.initiator_vc_marker = Some(vc_marker);
            // Store the peer cipher for later decryption of step 4
            self.initiator_decryptor = Some(peer_cipher);
        }

        self.phase = MseHandshakePhase::PublicKeyReceived;
        Ok(())
    }

    // ── Step 3 (initiator): Send negotiation ─────────────────────────

    /// Build the initiator's step 3 payload.
    ///
    /// Format: `Hash('req1', S) || [Hash('req2', SKEY) XOR Hash('req3', S)] || E(VC || crypto_provide || len(PadC) || PadC || len(IA) || IA)`
    ///
    /// Matches C++ `MSEHandshake::sendInitiatorStep2()`.
    pub fn build_initiator_step2(&mut self) -> Result<Vec<u8>, String> {
        if !self.initiator {
            return Err("Only initiator can build step 3".to_string());
        }
        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;

        let mut buf = Vec::new();

        // Hash('req1', S) — 20 bytes, plaintext
        buf.extend_from_slice(&keys.req1);

        // [Hash('req2', SKEY) XOR Hash('req3', S)] — 20 bytes, plaintext
        buf.extend_from_slice(&keys.req2_xor_req3());

        // Encrypted payload: VC(8) + crypto_provide(4) + len(PadC)(2) + PadC + len(IA)(2) + IA
        let mut rng = rand::thread_rng();
        let pad_c_length: u16 = rng.gen_range(0..=MAX_PAD_LENGTH as u16);
        let ia_length: u16 = 0; // No initial data currently

        let mut encrypted =
            Vec::with_capacity(VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 + pad_c_length as usize + 2);

        // VC = 8 zero bytes (will be encrypted by the cipher)
        encrypted.extend_from_slice(&[0u8; VC_LENGTH]);

        // crypto_provide = 4-byte big-endian bitmask
        let mut crypto_provide: u32 = 0;
        if !self.force_encryption && !self.prefer_encryption {
            crypto_provide |= MseCryptoMethod::Plain.as_u32();
        }
        crypto_provide |= MseCryptoMethod::Rc4.as_u32();
        encrypted.extend_from_slice(&crypto_provide.to_be_bytes());

        // len(PadC) — 2 bytes big-endian
        encrypted.extend_from_slice(&pad_c_length.to_be_bytes());

        // PadC — random bytes
        let mut pad_c = vec![0u8; pad_c_length as usize];
        rng.fill(&mut pad_c[..]);
        encrypted.extend_from_slice(&pad_c);

        // len(IA) — 2 bytes big-endian
        encrypted.extend_from_slice(&ia_length.to_be_bytes());

        // IA — currently empty (ia_length = 0)

        // Encrypt with the initiator's encryptor (keyA)
        if let Some(ref mut cipher) = self.initiator_encryptor {
            cipher.process(&mut encrypted);
        } else {
            return Err("Initiator encryptor not initialized".to_string());
        }

        buf.extend_from_slice(&encrypted);

        self.phase = MseHandshakePhase::InitiatorStep2Sent;
        Ok(buf)
    }

    // ── Step 3 (receiver): Process initiator's step 3 ────────────────

    /// Process the initiator's step 3 payload (receiver side).
    ///
    /// The receiver must find the req1 hash marker, verify req2^req3
    /// against known info_hashes, then decrypt the encrypted payload.
    ///
    /// Returns the crypto method provided by the initiator.
    ///
    /// Matches C++ `findReceiverHashMarker()` + `receiveReceiverHashAndPadCLength()`.
    pub fn receive_initiator_step2(
        &mut self,
        data: &[u8],
        known_info_hashes: &[[u8; INFO_HASH_LENGTH]],
    ) -> Result<MseCryptoMethod, String> {
        if self.initiator {
            return Err("Only receiver can process initiator step 3".to_string());
        }

        // Minimum size: req1(20) + req2^req3(20) + VC(8) + crypto_provide(4) + len(PadC)(2) = 54
        if data.len() < SHA1_LENGTH + SHA1_LENGTH + VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err(format!(
                "Initiator step3 data too short: {} bytes",
                data.len()
            ));
        }

        // Step 1: Find req1 hash marker in the data.
        // In C++ this uses std::search with a sync limit of 628 bytes.
        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;

        let req1_match = find_req1_marker(data, &keys.req1, RECEIVER_SYNC_LIMIT)?;
        let req1_end = req1_match + SHA1_LENGTH;

        // Step 2: Verify req2^req3 to identify the info_hash.
        // This allows multi-torrent support where the receiver doesn't know
        // which info_hash the initiator is using.
        let req2_xor_req3 = &data[req1_end..req1_end + SHA1_LENGTH];

        let matched_info_hash = verify_req2_xor_req3(req2_xor_req3, known_info_hashes, &keys.req3);

        // If we already know the info_hash, verify it matches
        let verified_info_hash = match matched_info_hash {
            Some(hash) => hash,
            None => {
                // Try our own info_hash
                let expected_xor = {
                    let mut expected = [0u8; SHA1_LENGTH];
                    for (expected, (&req2, &req3)) in expected
                        .iter_mut()
                        .zip(keys.req2.iter().zip(keys.req3.iter()))
                    {
                        *expected = req2 ^ req3;
                    }
                    expected
                };
                if req2_xor_req3 == expected_xor {
                    self.info_hash
                } else {
                    return Err("Unknown info hash: req2^req3 verification failed".to_string());
                }
            }
        };

        // If the matched info_hash differs from ours, update
        if verified_info_hash != self.info_hash {
            self.info_hash = verified_info_hash;
            // Re-derive keys with the correct info_hash
            let shared = self.shared_secret.ok_or("Shared secret not computed")?;
            let new_keys = MseDerivedKeys::derive(&shared, &verified_info_hash);
            self.keys = Some(new_keys);
        }

        let keys = self.keys.as_ref().expect("keys just set");

        // Step 3: Decrypt the encrypted payload.
        // The receiver's decryptor uses keyA (initiator's encryptor key).
        // Matches C++ initCipher() for receiver: decryptor = keyA.
        let mut decryptor = init_rc4(&keys.key_a);

        let encrypted_start = req1_end + SHA1_LENGTH;
        let encrypted_data = &data[encrypted_start..];

        if encrypted_data.len() < VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err("Encrypted payload too short".to_string());
        }

        // Decrypt the entire encrypted portion
        let mut decrypted = encrypted_data.to_vec();
        decryptor.process(&mut decrypted);
        self.receiver_decryptor = Some(decryptor);

        // Step 4: Verify VC (should be 8 zero bytes after decryption)
        let vc = &decrypted[..VC_LENGTH];
        if vc != [0u8; VC_LENGTH] {
            return Err(format!(
                "VC verification failed: expected zeros, got {:02X?}",
                &vc[..8.min(vc.len())]
            ));
        }

        // Step 5: Read crypto_provide
        let crypto_provide =
            u32::from_be_bytes([decrypted[8], decrypted[9], decrypted[10], decrypted[11]]);

        // Determine negotiated method
        if !self.prefer_encryption
            && !self.force_encryption
            && (crypto_provide & MseCryptoMethod::Plain.as_u32()) != 0
        {
            self.negotiated_method = MseCryptoMethod::Plain;
        } else if (crypto_provide & MseCryptoMethod::Rc4.as_u32()) != 0 {
            self.negotiated_method = MseCryptoMethod::Rc4;
        } else {
            return Err(format!(
                "No supported crypto method in provide: {:#010X}",
                crypto_provide
            ));
        }

        self.phase = MseHandshakePhase::Completed(self.negotiated_method);
        Ok(self.negotiated_method)
    }

    /// Return the complete receiver-side step-2 length once PadC and IA
    /// lengths are available, or `None` while more bytes are needed.
    ///
    /// The shared process listener uses this incremental seam before routing
    /// an encrypted connection. MSE deliberately hides the torrent identity
    /// until `req2 ^ req3` is verified against the active route catalog.
    pub fn receiver_step2_required_len(&self, data: &[u8]) -> Result<Option<usize>, String> {
        if self.initiator {
            return Err("Only receiver can inspect initiator step 2".to_string());
        }
        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;
        let Some(req1_match) = find_marker_if_present(data, &keys.req1) else {
            if data.len() >= RECEIVER_SYNC_LIMIT {
                return Err("Failed to find req1 hash marker within sync limit".to_string());
            }
            return Ok(None);
        };
        let encrypted_start = req1_match + SHA1_LENGTH + SHA1_LENGTH;
        let header_len = VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2;
        if data.len() < encrypted_start + header_len {
            return Ok(None);
        }

        let mut header = data[encrypted_start..encrypted_start + header_len].to_vec();
        let mut decryptor = init_rc4(&keys.key_a);
        decryptor.process(&mut header);
        if header[..VC_LENGTH] != [0u8; VC_LENGTH] {
            return Err("VC verification failed".to_string());
        }
        let pad_c_length = u16::from_be_bytes([
            header[VC_LENGTH + CRYPTO_BITFIELD_LENGTH],
            header[VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 1],
        ]) as usize;
        let ia_length_offset = encrypted_start + header_len + pad_c_length;
        if data.len() < ia_length_offset + 2 {
            return Ok(None);
        }

        let mut ia_length = [0u8; 2];
        let mut length_prefix = data[encrypted_start..ia_length_offset + 2].to_vec();
        let mut decryptor = init_rc4(&keys.key_a);
        decryptor.process(&mut length_prefix);
        ia_length.copy_from_slice(
            &length_prefix[ia_length_offset - encrypted_start
                ..ia_length_offset - encrypted_start + 2],
        );
        let ia_length = u16::from_be_bytes(ia_length) as usize;
        Ok(Some(ia_length_offset + 2 + ia_length))
    }

    /// Identify the concealed torrent identity in a receiver-side MSE
    /// buffer. The caller can use this before the encrypted payload is fully
    /// received, then call [`Self::set_info_hash`] to derive the final keys.
    pub fn receiver_info_hash(
        &self,
        data: &[u8],
        known_info_hashes: &[[u8; INFO_HASH_LENGTH]],
    ) -> Result<Option<[u8; INFO_HASH_LENGTH]>, String> {
        if self.initiator {
            return Err("Only receiver can identify an incoming info hash".to_string());
        }
        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;
        let Some(req1_match) = find_marker_if_present(data, &keys.req1) else {
            return Ok(None);
        };
        let xor_start = req1_match + SHA1_LENGTH;
        if data.len() < xor_start + SHA1_LENGTH {
            return Ok(None);
        }
        Ok(verify_req2_xor_req3(
            &data[xor_start..xor_start + SHA1_LENGTH],
            known_info_hashes,
            &keys.req3,
        ))
    }

    /// Select the torrent identity discovered from `req2 ^ req3` and derive
    /// the receiver-side MSE keys for the remainder of the handshake.
    pub fn set_info_hash(&mut self, info_hash: [u8; INFO_HASH_LENGTH]) -> Result<(), String> {
        if self.initiator {
            return Err("Only receiver can set an incoming info hash".to_string());
        }
        let shared = self.shared_secret.ok_or("Shared secret not computed")?;
        self.info_hash = info_hash;
        self.keys = Some(MseDerivedKeys::derive(&shared, &info_hash));
        Ok(())
    }

    // ── Step 4 (receiver): Send response ─────────────────────────────

    /// Build the receiver's step 4 response.
    ///
    /// Format: `E(VC || crypto_select || len(PadD) || PadD)`
    ///
    /// Matches C++ `MSEHandshake::sendReceiverStep2()`.
    pub fn build_receiver_step2(&mut self) -> Result<Vec<u8>, String> {
        if self.initiator {
            return Err("Only receiver can build step 4".to_string());
        }
        if !matches!(self.phase, MseHandshakePhase::Completed(_)) {
            return Err("Handshake not completed yet".to_string());
        }

        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;

        let mut rng = rand::thread_rng();
        let pad_d_length: u16 = rng.gen_range(0..=MAX_PAD_LENGTH as u16);

        let mut encrypted =
            Vec::with_capacity(VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 + pad_d_length as usize);

        // VC = 8 zero bytes
        encrypted.extend_from_slice(&[0u8; VC_LENGTH]);

        // crypto_select = 4-byte big-endian bitmask
        encrypted.extend_from_slice(&self.negotiated_method.as_u32().to_be_bytes());

        // len(PadD) = 2 bytes big-endian
        encrypted.extend_from_slice(&pad_d_length.to_be_bytes());

        // PadD = random bytes
        let mut pad_d = vec![0u8; pad_d_length as usize];
        rng.fill(&mut pad_d[..]);
        encrypted.extend_from_slice(&pad_d);

        // Encrypt with the receiver's encryptor (keyB for receiver)
        let cipher = self
            .receiver_encryptor
            .get_or_insert_with(|| init_rc4(&keys.key_b));
        cipher.process(&mut encrypted);

        Ok(encrypted)
    }

    // ── Step 4 (initiator): Process receiver's response ──────────────

    /// Process the receiver's step 4 response (initiator side).
    ///
    /// The initiator must find the VC marker, then decrypt crypto_select and PadD.
    ///
    /// Matches C++ `findInitiatorVCMarker()` + `receiveInitiatorCryptoSelectAndPadDLength()`.
    pub fn receive_receiver_step2(&mut self, data: &[u8]) -> Result<MseCryptoMethod, String> {
        if !self.initiator {
            return Err("Only initiator can process receiver step 4".to_string());
        }

        let vc_marker = self.initiator_vc_marker.ok_or("VC marker not computed")?;

        // Find the VC marker in the data
        let vc_pos = find_vc_marker(data, &vc_marker, INITIATOR_SYNC_LIMIT)?;

        // Decrypt everything after the VC marker using the initiator's decryptor
        let encrypted_start = vc_pos + VC_LENGTH;
        if data.len() < encrypted_start + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err(format!(
                "Receiver step4 data too short after VC: {} bytes",
                data.len() - encrypted_start
            ));
        }

        let mut remaining = data[encrypted_start..].to_vec();
        if let Some(ref mut cipher) = self.initiator_decryptor {
            cipher.process(&mut remaining);
        } else {
            return Err("Initiator decryptor not initialized".to_string());
        }

        // Read crypto_select (4 bytes big-endian)
        let crypto_select =
            u32::from_be_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]);

        // Determine negotiated method
        if (crypto_select & MseCryptoMethod::Plain.as_u32()) != 0 && !self.force_encryption {
            self.negotiated_method = MseCryptoMethod::Plain;
        } else if (crypto_select & MseCryptoMethod::Rc4.as_u32()) != 0 {
            self.negotiated_method = MseCryptoMethod::Rc4;
        } else {
            return Err(format!(
                "No supported crypto method in select: {:#010X}",
                crypto_select
            ));
        }

        // Read PadD length (2 bytes big-endian)
        if remaining.len() < CRYPTO_BITFIELD_LENGTH + 2 {
            return Err("Not enough data for PadD length".to_string());
        }
        let _pad_d_length = u16::from_be_bytes([remaining[4], remaining[5]]);
        if _pad_d_length as usize > MAX_PAD_LENGTH {
            return Err(format!("PadD length too large: {}", _pad_d_length));
        }

        self.phase = MseHandshakePhase::Completed(self.negotiated_method);
        Ok(self.negotiated_method)
    }

    /// Return the exact receiver response length once PadD is available.
    ///
    /// The response can be preceded by responder padding, so callers must
    /// synchronize on the encrypted VC marker before decoding the encrypted
    /// PadD length. This keeps the following BitTorrent handshake on the
    /// stream for the next protocol phase.
    pub fn initiator_step2_required_len(&self, data: &[u8]) -> Result<Option<usize>, String> {
        if !self.initiator {
            return Err("Only initiator can inspect receiver step 2".to_string());
        }
        let vc_marker = self.initiator_vc_marker.ok_or("VC marker not computed")?;
        let Some(vc_pos) = find_marker_if_present(data, &vc_marker) else {
            if data.len() >= INITIATOR_SYNC_LIMIT {
                return Err("Failed to find VC marker within sync limit".to_string());
            }
            return Ok(None);
        };
        let header_start = vc_pos + VC_LENGTH;
        if data.len() < header_start + CRYPTO_BITFIELD_LENGTH + 2 {
            return Ok(None);
        }

        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;
        let mut decryptor = init_rc4(&keys.key_b);
        let mut vc = [0u8; VC_LENGTH];
        decryptor.process(&mut vc);
        let mut header = data[header_start..header_start + CRYPTO_BITFIELD_LENGTH + 2].to_vec();
        decryptor.process(&mut header);
        let pad_d_length = u16::from_be_bytes([
            header[CRYPTO_BITFIELD_LENGTH],
            header[CRYPTO_BITFIELD_LENGTH + 1],
        ]) as usize;
        Ok(Some(header_start + CRYPTO_BITFIELD_LENGTH + 2 + pad_d_length))
    }

    // ── Finalize: Extract crypto state ───────────────────────────────

    /// Finalize the handshake and return the ongoing crypto state.
    ///
    /// The returned `MseCryptoState` can be used for encrypting/decrypting
    /// all subsequent BT protocol messages.
    pub fn finalize(mut self) -> Result<MseCryptoState, String> {
        match self.phase {
            MseHandshakePhase::Completed(method) => {
                match method {
                    MseCryptoMethod::Plain => Ok(MseCryptoState::new_plain()),
                    MseCryptoMethod::Rc4 if self.initiator => {
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
                    MseCryptoMethod::Rc4 => {
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
                }
            }
            MseHandshakePhase::Failed(e) => Err(e),
            _ => Err(format!("Handshake not completed: {:?}", self.phase)),
        }
    }

    // ── Utility ──────────────────────────────────────────────────────

    /// Determine whether MSE should be negotiated based on reserved bytes.
    ///
    /// Convention: `reserved[7] & 0x01` indicates MSE support.
    pub fn should_negotiate(local_supports_mse: bool, remote_reserved: &[u8]) -> bool {
        local_supports_mse && remote_reserved.len() >= 8 && (remote_reserved[7] & 0x01) != 0
    }

    /// Identify whether incoming data is a legacy BT handshake or encrypted.
    ///
    /// Matches C++ `MSEHandshake::identifyHandshakeType()`.
    pub fn identify_handshake_type(data: &[u8]) -> HandshakeType {
        if data.len() < 20 {
            return HandshakeType::NotYet;
        }
        // Legacy BT handshake: first byte = 19 (pstrlen), next 19 bytes = "BitTorrent protocol"
        if data[0] == 19 && &data[1..20] == b"BitTorrent protocol" {
            return HandshakeType::Legacy;
        }
        HandshakeType::Encrypted
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

/// Search for the req1 hash marker in the data buffer.
///
/// Returns the byte offset where the marker starts.
/// Matches C++ `findReceiverHashMarker()`.
fn find_req1_marker(
    data: &[u8],
    req1_hash: &[u8; SHA1_LENGTH],
    sync_limit: usize,
) -> Result<usize, String> {
    if data.len() < SHA1_LENGTH {
        return Err("Data too short for req1 marker search".to_string());
    }

    // Search for the req1 hash in the data
    for i in 0..=(data.len().saturating_sub(SHA1_LENGTH)) {
        if data[i..].starts_with(req1_hash) {
            return Ok(i);
        }
        // Sync limit check
        if i + SHA1_LENGTH > sync_limit {
            return Err("Failed to find req1 hash marker within sync limit".to_string());
        }
    }

    Err("Failed to find req1 hash marker".to_string())
}

fn find_marker_if_present(data: &[u8], marker: &[u8]) -> Option<usize> {
    data.windows(marker.len()).position(|window| window == marker)
}

/// Find the VC marker in the receiver's response data.
///
/// Returns the byte offset where the VC marker starts.
/// Matches C++ `findInitiatorVCMarker()`.
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
        // Sync limit check (adjusted for KEY_LENGTH offset as in C++)
        if i + VC_LENGTH > sync_limit - KEY_LENGTH {
            return Err("Failed to find VC marker within sync limit".to_string());
        }
    }

    Err("Failed to find VC marker".to_string())
}

/// Verify req2^req3 against a list of known info_hashes.
///
/// Returns the matching info_hash if found, or None.
/// Matches C++ `receiveReceiverHashAndPadCLength()`.
fn verify_req2_xor_req3(
    req2_xor_req3: &[u8],
    known_info_hashes: &[[u8; INFO_HASH_LENGTH]],
    req3: &[u8; SHA1_LENGTH],
) -> Option<[u8; INFO_HASH_LENGTH]> {
    for info_hash in known_info_hashes {
        // Compute Hash('req2', SKEY)
        let req2 = {
            use sha1::{Digest, Sha1};
            let mut hasher = Sha1::new();
            hasher.update(b"req2");
            hasher.update(info_hash);
            let result = hasher.finalize();
            let mut arr = [0u8; SHA1_LENGTH];
            arr.copy_from_slice(&result);
            arr
        };

        // Compute Hash('req2', SKEY) XOR Hash('req3', S)
        let mut expected = [0u8; SHA1_LENGTH];
        for i in 0..SHA1_LENGTH {
            expected[i] = req2[i] ^ req3[i];
        }

        if req2_xor_req3 == expected {
            return Some(*info_hash);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_info_hash() -> [u8; INFO_HASH_LENGTH] {
        [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54,
            0x32, 0x10, 0xAA, 0xBB, 0xCC, 0xDD,
        ]
    }

    // ── Step 1: Public key exchange ──────────────────────────────────

    #[test]
    fn test_step1_payload_size() {
        let h = MseHandshake::new_initiator(test_info_hash());
        let s1 = h.build_step1();
        // Must be at least 96 bytes (public key) and at most 96 + 512
        assert!(s1.len() >= KEY_LENGTH);
        assert!(s1.len() <= KEY_LENGTH + MAX_PAD_LENGTH);
    }

    #[test]
    fn test_receive_step1_too_short() {
        let mut h = MseHandshake::new_responder(test_info_hash());
        let result = h.receive_step1(&[0u8; 16]);
        assert!(result.is_err());
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

    // ── Full initiator-responder handshake ───────────────────────────

    #[test]
    fn test_full_handshake_rc4() {
        let info_hash = test_info_hash();

        // Create initiator and responder
        let mut initiator = MseHandshake::new_initiator(info_hash);
        let mut responder = MseHandshake::new_responder(info_hash);

        // Step 1: Exchange public keys
        let i_step1 = initiator.build_step1();
        let r_step1 = responder.build_step1();

        initiator
            .receive_step1(&r_step1)
            .expect("I receive R step1");
        responder
            .receive_step1(&i_step1)
            .expect("R receive I step1");

        // Verify both computed the same shared secret
        let i_secret = initiator.shared_secret.expect("I secret");
        let r_secret = responder.shared_secret.expect("R secret");
        assert_eq!(i_secret, r_secret, "Shared secrets must match");

        // Step 3: Initiator sends negotiation
        let i_step3 = initiator.build_initiator_step2().expect("I build step3");

        // Receiver processes initiator's step 3
        let method_r = responder
            .receive_initiator_step2(&i_step3, &[info_hash])
            .expect("R receive step3");
        assert_eq!(method_r, MseCryptoMethod::Rc4);

        // Step 4: Receiver sends response
        let r_step4 = responder.build_receiver_step2().expect("R build step4");

        // Initiator processes receiver's step 4
        let method_i = initiator
            .receive_receiver_step2(&r_step4)
            .expect("I receive step4");
        assert_eq!(method_i, MseCryptoMethod::Rc4);

        // Finalize both sides
        let mut crypto_i = initiator.finalize().expect("I finalize");
        let crypto_r = responder.finalize().expect("R finalize");

        assert!(crypto_i.is_encrypted());
        assert!(crypto_r.is_encrypted());

        // Verify encrypted communication works
        let original = b"Hello, BitTorrent MSE!";
        let mut encrypted = original.to_vec();
        crypto_i.encrypt(&mut encrypted);
        // Create a fresh pair since Clone loses cipher state
        let info_hash2 = test_info_hash();
        let mut initiator2 = MseHandshake::new_initiator(info_hash2);
        let mut responder2 = MseHandshake::new_responder(info_hash2);
        let i2_step1 = initiator2.build_step1();
        let r2_step1 = responder2.build_step1();
        initiator2
            .receive_step1(&r2_step1)
            .expect("I2 receive R2 step1");
        responder2
            .receive_step1(&i2_step1)
            .expect("R2 receive I2 step1");
        let i2_step3 = initiator2.build_initiator_step2().expect("I2 build step3");
        responder2
            .receive_initiator_step2(&i2_step3, &[test_info_hash()])
            .expect("R2 receive step3");
        let r2_step4 = responder2.build_receiver_step2().expect("R2 build step4");
        initiator2
            .receive_receiver_step2(&r2_step4)
            .expect("I2 receive step4");
        let mut crypto_i2 = initiator2.finalize().expect("I2 finalize");
        let mut crypto_r2 = responder2.finalize().expect("R2 finalize");

        let original2 = b"Test encrypted data";
        let mut enc2 = original2.to_vec();
        crypto_i2.encrypt(&mut enc2);
        assert_ne!(enc2, original2.to_vec());
        crypto_r2.decrypt(&mut enc2);
        assert_eq!(enc2, original2.to_vec());
    }

    #[test]
    fn test_handshake_type_detection() {
        // Legacy BT handshake: pstrlen=19 + "BitTorrent protocol"
        let mut legacy = vec![19u8];
        legacy.extend_from_slice(b"BitTorrent protocol");
        assert_eq!(
            MseHandshake::identify_handshake_type(&legacy),
            HandshakeType::Legacy
        );

        // Encrypted: first byte is not 19
        let encrypted = [0u8; 20];
        assert_eq!(
            MseHandshake::identify_handshake_type(&encrypted),
            HandshakeType::Encrypted
        );

        // Not enough data
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

        let pk1 = h1.dh.generate_public_key();
        let pk2 = h2.dh.generate_public_key();

        assert_ne!(
            pk1, pk2,
            "Different instances should have different public keys"
        );
    }

    #[test]
    fn test_initiator_step2_before_keys() {
        let mut h = MseHandshake::new_initiator(test_info_hash());
        let result = h.build_initiator_step2();
        assert!(result.is_err());
    }

    #[test]
    fn test_finalize_before_completed() {
        let h = MseHandshake::new_initiator(test_info_hash());
        let result = h.finalize();
        assert!(result.is_err());
    }

    #[test]
    fn test_receiver_step4_before_completed() {
        let mut h = MseHandshake::new_responder(test_info_hash());
        let result = h.build_receiver_step2();
        assert!(result.is_err());
    }

    #[test]
    fn test_only_initiator_builds_step3() {
        let mut h = MseHandshake::new_responder(test_info_hash());
        let result = h.build_initiator_step2();
        assert!(result.is_err());
    }

    #[test]
    fn test_only_receiver_builds_step4() {
        let info_hash = test_info_hash();
        let mut h = MseHandshake::new_initiator(info_hash);
        let result = h.build_receiver_step2();
        assert!(result.is_err());
    }

    #[test]
    fn test_only_receiver_processes_step3() {
        let mut h = MseHandshake::new_initiator(test_info_hash());
        let result = h.receive_initiator_step2(&[], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_only_initiator_processes_step4() {
        let mut h = MseHandshake::new_responder(test_info_hash());
        let result = h.receive_receiver_step2(&[]);
        assert!(result.is_err());
    }

    // ── Key derivation matches C++ format ────────────────────────────

    #[test]
    fn test_key_derivation_format() {
        let info_hash = test_info_hash();
        let initiator = MseHandshake::new_initiator(info_hash);
        let step1 = initiator.build_step1();

        let mut responder = MseHandshake::new_responder(info_hash);
        responder.receive_step1(&step1).expect("receive step1");

        let keys = responder.keys.as_ref().expect("keys derived");

        // Verify keyA format: SHA1("keyA" || S || infoHash)
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

    // ── End-to-end with multiple info_hashes ─────────────────────────

    #[test]
    fn test_multi_torrent_info_hash_selection() {
        let info_hash_1 = [0x01u8; INFO_HASH_LENGTH];
        let info_hash_2 = [0x02u8; INFO_HASH_LENGTH];

        // Initiator wants info_hash_2
        let mut initiator = MseHandshake::new_initiator(info_hash_2);
        // Responder knows about both torrents
        let mut responder = MseHandshake::new_responder(info_hash_1);

        // Exchange public keys
        let i_step1 = initiator.build_step1();
        let r_step1 = responder.build_step1();
        initiator
            .receive_step1(&r_step1)
            .expect("I receive R step1");
        responder
            .receive_step1(&i_step1)
            .expect("R receive I step1");

        // Initiator sends step 3 with info_hash_2
        let i_step3 = initiator.build_initiator_step2().expect("I build step3");

        // Responder verifies against known info_hashes and finds info_hash_2
        let method = responder
            .receive_initiator_step2(&i_step3, &[info_hash_1, info_hash_2])
            .expect("R receive step3");
        assert_eq!(method, MseCryptoMethod::Rc4);

        // Responder should now be using info_hash_2
        assert_eq!(responder.info_hash, info_hash_2);
    }
}
