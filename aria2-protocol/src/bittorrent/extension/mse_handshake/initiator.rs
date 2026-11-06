use rand::Rng;

use super::{
    CRYPTO_BITFIELD_LENGTH, INITIATOR_SYNC_LIMIT, MAX_PAD_LENGTH, MseCryptoMethod, MseHandshake,
    MseHandshakePhase, VC_LENGTH, find_marker_if_present, find_vc_marker, init_rc4,
};

impl MseHandshake {
    /// Build the initiator's step 3 payload.
    ///
    /// Matches C++ `MSEHandshake::sendInitiatorStep2()`.
    pub fn build_initiator_step2(&mut self) -> Result<Vec<u8>, String> {
        if !self.initiator {
            return Err("Only initiator can build step 3".to_string());
        }
        let keys = self.keys.as_ref().ok_or("Keys not derived yet")?;
        let mut buf = Vec::new();
        buf.extend_from_slice(&keys.req1);
        buf.extend_from_slice(&keys.req2_xor_req3());

        let mut rng = rand::thread_rng();
        let pad_c_length: u16 = rng.gen_range(0..=MAX_PAD_LENGTH as u16);
        let mut encrypted =
            Vec::with_capacity(VC_LENGTH + CRYPTO_BITFIELD_LENGTH + 2 + pad_c_length as usize + 2);
        encrypted.extend_from_slice(&[0u8; VC_LENGTH]);

        let mut crypto_provide = 0;
        if !self.force_encryption && !self.prefer_encryption {
            crypto_provide |= MseCryptoMethod::Plain.as_u32();
        }
        crypto_provide |= MseCryptoMethod::Rc4.as_u32();
        encrypted.extend_from_slice(&crypto_provide.to_be_bytes());
        encrypted.extend_from_slice(&pad_c_length.to_be_bytes());
        let mut pad_c = vec![0u8; pad_c_length as usize];
        rng.fill(&mut pad_c[..]);
        encrypted.extend_from_slice(&pad_c);
        encrypted.extend_from_slice(&0u16.to_be_bytes());

        self.initiator_encryptor
            .as_mut()
            .ok_or("Initiator encryptor not initialized")?
            .process(&mut encrypted);
        buf.extend_from_slice(&encrypted);
        self.phase = MseHandshakePhase::InitiatorStep2Sent;
        Ok(buf)
    }

    /// Process the receiver's step 4 response.
    ///
    /// Matches C++ `findInitiatorVCMarker()` and
    /// `receiveInitiatorCryptoSelectAndPadDLength()`.
    pub fn receive_receiver_step2(&mut self, data: &[u8]) -> Result<MseCryptoMethod, String> {
        if !self.initiator {
            return Err("Only initiator can process receiver step 4".to_string());
        }
        let vc_marker = self.initiator_vc_marker.ok_or("VC marker not computed")?;
        let vc_pos = find_vc_marker(data, &vc_marker, INITIATOR_SYNC_LIMIT)?;

        let mut vc = [0u8; VC_LENGTH];
        vc.copy_from_slice(&data[vc_pos..vc_pos + VC_LENGTH]);
        self.initiator_decryptor
            .as_mut()
            .ok_or("Initiator decryptor not initialized")?
            .process(&mut vc);
        if vc != [0u8; VC_LENGTH] {
            return Err("VC verification failed".to_string());
        }

        let encrypted_start = vc_pos + VC_LENGTH;
        if data.len() < encrypted_start + CRYPTO_BITFIELD_LENGTH + 2 {
            return Err(format!(
                "Receiver step4 data too short after VC: {} bytes",
                data.len() - encrypted_start
            ));
        }
        let mut remaining = data[encrypted_start..].to_vec();
        self.initiator_decryptor
            .as_mut()
            .ok_or("Initiator decryptor not initialized")?
            .process(&mut remaining);

        let crypto_select =
            u32::from_be_bytes([remaining[0], remaining[1], remaining[2], remaining[3]]);
        if (crypto_select & MseCryptoMethod::Plain.as_u32()) != 0
            && !self.force_encryption
            && !self.prefer_encryption
        {
            self.negotiated_method = MseCryptoMethod::Plain;
        } else if (crypto_select & MseCryptoMethod::Rc4.as_u32()) != 0 {
            self.negotiated_method = MseCryptoMethod::Rc4;
        } else {
            return Err(format!(
                "No supported crypto method in select: {crypto_select:#010X}"
            ));
        }

        let pad_d_length = u16::from_be_bytes([remaining[4], remaining[5]]) as usize;
        if pad_d_length > MAX_PAD_LENGTH {
            return Err(format!("PadD length too large: {pad_d_length}"));
        }
        self.phase = MseHandshakePhase::Completed(self.negotiated_method);
        Ok(self.negotiated_method)
    }

    /// Return the exact receiver response length once PadD is available.
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
        if pad_d_length > MAX_PAD_LENGTH {
            return Err(format!("PadD length too large: {pad_d_length}"));
        }
        Ok(Some(
            header_start + CRYPTO_BITFIELD_LENGTH + 2 + pad_d_length,
        ))
    }
}
