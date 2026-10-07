use rand::Rng;

use super::{
    CRYPTO_BITFIELD_LENGTH, MAX_INITIAL_DATA_LENGTH, MAX_PAD_LENGTH, MseCryptoMethod, MseHandshake,
    MseHandshakePhase, RECEIVER_SYNC_LIMIT, SHA1_LENGTH, VC_LENGTH, find_marker_if_present,
    find_req1_marker, init_rc4, verify_req2_xor_req3,
};
use crate::bittorrent::extension::mse_crypto::MseDerivedKeys;
use crate::bittorrent::extension::mse_dh::INFO_HASH_LENGTH;

impl MseHandshake {
    /// Process the initiator's step 3 payload and negotiate its crypto method.
    pub fn receive_initiator_step2(
        &mut self,
        data: &[u8],
        known_info_hashes: &[[u8; INFO_HASH_LENGTH]],
    ) -> Result<MseCryptoMethod, String> {
        if self.initiator {
            return Err("Only receiver can process initiator step 3".to_string());
        }
        if data.len() < SHA1_LENGTH + SHA1_LENGTH + VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err(format!(
                "Initiator step3 data too short: {} bytes",
                data.len()
            ));
        }

        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;
        let req1_match = find_req1_marker(data, &keys.req1, RECEIVER_SYNC_LIMIT)?;
        let req1_end = req1_match + SHA1_LENGTH;
        let req2_xor_req3 = &data[req1_end..req1_end + SHA1_LENGTH];
        let verified_info_hash =
            match verify_req2_xor_req3(req2_xor_req3, known_info_hashes, &keys.req3) {
                Some(hash) => hash,
                None => {
                    let mut expected_xor = [0u8; SHA1_LENGTH];
                    for (expected, (&req2, &req3)) in expected_xor
                        .iter_mut()
                        .zip(keys.req2.iter().zip(keys.req3.iter()))
                    {
                        *expected = req2 ^ req3;
                    }
                    if req2_xor_req3 == expected_xor {
                        self.info_hash
                    } else {
                        return Err("Unknown info hash: req2^req3 verification failed".to_string());
                    }
                }
            };

        if verified_info_hash != self.info_hash {
            self.info_hash = verified_info_hash;
            let shared = self.shared_secret.ok_or("Shared secret not computed")?;
            self.keys = Some(MseDerivedKeys::derive(&shared, &verified_info_hash));
        }
        let keys = self.keys.as_ref().expect("keys just set");
        let encrypted_start = req1_end + SHA1_LENGTH;
        let encrypted_data = &data[encrypted_start..];
        if encrypted_data.len() < VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err("Encrypted payload too short".to_string());
        }

        let mut decryptor = init_rc4(&keys.key_a);
        let mut decrypted = encrypted_data.to_vec();
        decryptor.process(&mut decrypted);
        if decrypted[..VC_LENGTH] != [0u8; VC_LENGTH] {
            return Err(format!(
                "VC verification failed: expected zeros, got {:02X?}",
                &decrypted[..VC_LENGTH]
            ));
        }

        let crypto_provide =
            u32::from_be_bytes([decrypted[8], decrypted[9], decrypted[10], decrypted[11]]);
        let pad_c_length = u16::from_be_bytes([decrypted[12], decrypted[13]]) as usize;
        if pad_c_length > MAX_PAD_LENGTH {
            return Err(format!("PadC length too large: {pad_c_length}"));
        }
        let ia_length_offset = VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 + pad_c_length;
        if decrypted.len() < ia_length_offset + 2 {
            return Err("Encrypted payload is truncated before IA length".to_string());
        }
        let ia_length =
            u16::from_be_bytes([decrypted[ia_length_offset], decrypted[ia_length_offset + 1]])
                as usize;
        if ia_length > MAX_INITIAL_DATA_LENGTH {
            return Err(format!("IA length too large: {ia_length}"));
        }
        let ia_start = ia_length_offset + 2;
        let ia_end = ia_start + ia_length;
        if decrypted.len() < ia_end {
            return Err("Encrypted payload is truncated before IA data".to_string());
        }
        self.receiver_initial_payload = decrypted[ia_start..ia_end].to_vec();

        if !self.prefer_encryption
            && !self.force_encryption
            && crypto_provide & MseCryptoMethod::Plain.as_u32() != 0
        {
            self.negotiated_method = MseCryptoMethod::Plain;
        } else if crypto_provide & MseCryptoMethod::Rc4.as_u32() != 0 {
            self.negotiated_method = MseCryptoMethod::Rc4;
        } else {
            return Err(format!(
                "No supported crypto method in provide: {crypto_provide:#010X}"
            ));
        }

        self.receiver_decryptor = Some(decryptor);
        self.phase = MseHandshakePhase::Completed(self.negotiated_method);
        Ok(self.negotiated_method)
    }

    /// Return the complete initiator step-3 length after PadC and IA lengths arrive.
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
        init_rc4(&keys.key_a).process(&mut header);
        if header[..VC_LENGTH] != [0u8; VC_LENGTH] {
            return Err("VC verification failed".to_string());
        }
        let pad_c_length = u16::from_be_bytes([
            header[VC_LENGTH + CRYPTO_BITFIELD_LENGTH],
            header[VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 1],
        ]) as usize;
        if pad_c_length > MAX_PAD_LENGTH {
            return Err(format!("PadC length too large: {pad_c_length}"));
        }
        let ia_length_offset = encrypted_start + header_len + pad_c_length;
        if data.len() < ia_length_offset + 2 {
            return Ok(None);
        }

        let mut length_prefix = data[encrypted_start..ia_length_offset + 2].to_vec();
        init_rc4(&keys.key_a).process(&mut length_prefix);
        let ia_length_offset_in_prefix = ia_length_offset - encrypted_start;
        let ia_length = u16::from_be_bytes([
            length_prefix[ia_length_offset_in_prefix],
            length_prefix[ia_length_offset_in_prefix + 1],
        ]) as usize;
        if ia_length > MAX_INITIAL_DATA_LENGTH {
            return Err(format!("IA length too large: {ia_length}"));
        }
        Ok(Some(ia_length_offset + 2 + ia_length))
    }

    /// Identify the concealed torrent from req2^req3 before reading the payload.
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

    /// Select the torrent identity discovered from req2^req3.
    pub fn set_info_hash(&mut self, info_hash: [u8; INFO_HASH_LENGTH]) -> Result<(), String> {
        if self.initiator {
            return Err("Only receiver can set an incoming info hash".to_string());
        }
        let shared = self.shared_secret.ok_or("Shared secret not computed")?;
        self.info_hash = info_hash;
        self.keys = Some(MseDerivedKeys::derive(&shared, &info_hash));
        Ok(())
    }

    /// Build the receiver's step 4 response.
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
        encrypted.extend_from_slice(&[0u8; VC_LENGTH]);
        encrypted.extend_from_slice(&self.negotiated_method.as_u32().to_be_bytes());
        encrypted.extend_from_slice(&pad_d_length.to_be_bytes());
        let mut pad_d = vec![0u8; pad_d_length as usize];
        rng.fill(&mut pad_d[..]);
        encrypted.extend_from_slice(&pad_d);
        self.receiver_encryptor
            .get_or_insert_with(|| init_rc4(&keys.key_b))
            .process(&mut encrypted);
        Ok(encrypted)
    }

    /// Take the decrypted IA bytes after processing the initiator's step 3.
    pub(crate) fn take_receiver_initial_payload(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.receiver_initial_payload)
    }
}
