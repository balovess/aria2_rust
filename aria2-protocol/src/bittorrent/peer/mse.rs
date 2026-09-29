use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

use crate::bittorrent::extension::mse_handshake::{MSE_PUBLIC_KEY_LENGTH, MseHandshake};
use crate::bittorrent::message::handshake::Handshake;
use crate::bittorrent::peer::connection::PeerConnection;

#[derive(Debug, Clone, Copy)]
pub struct MseConnectionOptions {
    pub force_encryption: bool,
    pub prefer_encryption: bool,
    pub local_peer_id: [u8; 20],
    pub timeout: std::time::Duration,
    pub dht_enabled: bool,
}

/// Complete MSE over a TCP stream selected by the caller and return the
/// regular peer connection, which owns encryption and BitTorrent framing.
pub async fn connect_with_stream(
    mut stream: tokio::net::TcpStream,
    info_hash: &[u8; 20],
    info_hash_v2: Option<&[u8; 32]>,
    options: MseConnectionOptions,
) -> Result<PeerConnection, String> {
    let mut initiator = MseHandshake::new_initiator(*info_hash);
    initiator.set_crypto_preferences(options.force_encryption, options.prefer_encryption);

    // Step 1: Exchange DH public keys
    let step1_i = initiator.build_step1();
    stream
        .write_all(&step1_i)
        .await
        .map_err(|e| format!("MSE Step1 send failed: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("MSE Step1 flush failed: {}", e))?;

    // PadB has no explicit length. The later VC marker synchronizes the
    // response, just as MSEHandshake::findInitiatorVCMarker does upstream.
    let mut step1_r_buf = vec![0u8; MSE_PUBLIC_KEY_LENGTH];
    read_exact_with_timeout(
        &mut stream,
        &mut step1_r_buf,
        "MSE public key",
        options.timeout,
    )
    .await?;

    initiator.receive_step1(&step1_r_buf)?;

    // Step 3 (initiator): Send req1 + req2^req3 + encrypted payload
    let step3_i = initiator.build_initiator_step2()?;
    stream
        .write_all(&step3_i)
        .await
        .map_err(|e| format!("MSE Step3 send failed: {}", e))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("MSE Step3 flush failed: {}", e))?;

    let mut step4_r_buf = Vec::with_capacity(1_152);
    let mut read_ahead = Vec::new();
    let response_len = loop {
        let mut chunk = [0u8; 64];
        let read_len = tokio::time::timeout(options.timeout, stream.read(&mut chunk))
            .await
            .map_err(|_| "MSE response read timeout".to_string())?
            .map_err(|error| format!("MSE response read failed: {error}"))?;
        if read_len == 0 {
            return Err("MSE response: peer closed connection".to_string());
        }
        step4_r_buf.extend_from_slice(&chunk[..read_len]);
        if let Some(length) = initiator.initiator_step2_required_len(&step4_r_buf)?
            && step4_r_buf.len() >= length
        {
            if step4_r_buf.len() > length {
                read_ahead.extend_from_slice(&step4_r_buf.split_off(length));
            }
            break length;
        }
        if step4_r_buf.len() >= 1_152 {
            return Err("MSE response exceeded handshake buffer limit".to_string());
        }
    };
    step4_r_buf.truncate(response_len);
    initiator.receive_receiver_step2(&step4_r_buf)?;
    let mut crypto = initiator.finalize()?;

    info!(
        "MSE handshake complete: encrypted={}",
        crypto.is_encrypted()
    );

    let mut local_handshake = Handshake::new(info_hash, &options.local_peer_id)
        .with_dht(options.dht_enabled)
        .with_bep52(info_hash_v2.is_some())
        .to_bytes();
    crypto.encrypt(&mut local_handshake);
    stream
        .write_all(&local_handshake)
        .await
        .map_err(|error| format!("Failed to send encrypted handshake: {error}"))?;

    let mut remote_handshake = [0u8; 68];
    read_exact_with_pending(
        &mut stream,
        &mut read_ahead,
        &mut remote_handshake,
        "MSE handshake",
        options.timeout,
    )
    .await?;
    crypto.decrypt(&mut remote_handshake);
    let remote_hs = Handshake::parse(&remote_handshake).map_err(|error| {
        format!(
            "{error}; decrypted handshake prefix={:02x?}",
            &remote_handshake[..8]
        )
    })?;
    if remote_hs.info_hash != *info_hash {
        let Some(info_hash_v2) = info_hash_v2 else {
            return Err("info_hash mismatch".to_string());
        };
        let v2_truncated: [u8; 20] = info_hash_v2[..20]
            .try_into()
            .expect("SHA-256 hash is 32 bytes");
        if remote_hs.info_hash != v2_truncated || !remote_hs.supports_bep52() {
            return Err(
                "hybrid handshake info_hash mismatch or missing BEP 52 capability".to_string(),
            );
        }
    }
    Ok(PeerConnection::from_stream_with_mse(
        stream,
        crypto,
        read_ahead,
        remote_hs.peer_id,
        remote_hs.supports_dht(),
        remote_hs.supports_fast_extension(),
    ))
}

async fn read_exact_with_timeout(
    stream: &mut tokio::net::TcpStream,
    buffer: &mut [u8],
    label: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    tokio::time::timeout(timeout, stream.read_exact(buffer))
        .await
        .map_err(|_| format!("{label} read timeout"))?
        .map(|_| ())
        .map_err(|error| format!("{label} read failed: {error}"))
}

async fn read_exact_with_pending(
    stream: &mut tokio::net::TcpStream,
    pending: &mut Vec<u8>,
    buffer: &mut [u8],
    label: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let copied = pending.len().min(buffer.len());
    buffer[..copied].copy_from_slice(&pending[..copied]);
    pending.drain(..copied);
    if copied < buffer.len() {
        read_exact_with_timeout(stream, &mut buffer[copied..], label, timeout).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_negotiate_all_combos() {
        // MSE reserved bit is at reserved[7] bit 0
        let reserved_zero = [0u8; 8];
        let mut reserved_mse = [0u8; 8];
        reserved_mse[7] = 0x01;
        let mut reserved_ff = [0u8; 8];
        reserved_ff[7] = 0xFF;

        assert!(!MseHandshake::should_negotiate(true, &reserved_zero));
        assert!(MseHandshake::should_negotiate(true, &reserved_mse));
        assert!(MseHandshake::should_negotiate(true, &reserved_ff));
        assert!(!MseHandshake::should_negotiate(false, &reserved_mse));
        assert!(!MseHandshake::should_negotiate(true, &[]));
    }
}
