//! Incoming BitTorrent handshake adapter.
//!
//! This is the protocol seam used by the process-level listener. It hides the
//! difference between a legacy 68-byte handshake and aria2-compatible MSE,
//! while leaving route selection to the caller through `info_hash()`.

use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bittorrent::extension::mse_handshake::{
    MSE_MAX_BUFFER_LENGTH, MSE_PUBLIC_KEY_LENGTH, MseHandshake,
};
use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::message::handshake::Handshake;

const HANDSHAKE_LENGTH: usize = 68;
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Per-torrent policy applied after an incoming MSE handshake reveals the
/// info-hash. This keeps route selection independent from download options.
#[derive(Debug, Clone, Copy, Default)]
pub struct IncomingCryptoPolicy {
    /// Reject the legacy unencrypted BitTorrent handshake.
    pub reject_plain: bool,
    /// Do not negotiate plaintext after MSE.
    pub force_encryption: bool,
    /// Prefer RC4 when both MSE methods are offered.
    pub prefer_encryption: bool,
}

pub enum IncomingConnection {
    Plain(crate::bittorrent::peer::connection::PeerConnection),
    Encrypted(crate::bittorrent::peer::encrypted_connection::EncryptedConnection),
}

impl IncomingConnection {
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        match self {
            Self::Plain(connection) => connection.remote_peer_id,
            Self::Encrypted(connection) => connection.remote_peer_id().copied(),
        }
    }
}

pub enum IncomingHandshake {
    Plain {
        stream: TcpStream,
        handshake: Handshake,
    },
    Mse {
        stream: TcpStream,
        handshake: Handshake,
        crypto: MseCryptoState,
    },
}

impl IncomingHandshake {
    pub fn info_hash(&self) -> &[u8; 20] {
        match self {
            Self::Plain { handshake, .. } | Self::Mse { handshake, .. } => &handshake.info_hash,
        }
    }

    pub async fn complete(self, local_peer_id: [u8; 20]) -> Result<IncomingConnection, String> {
        match self {
            Self::Plain {
                mut stream,
                handshake,
            } => {
                stream
                    .write_all(&Handshake::new(&handshake.info_hash, &local_peer_id).to_bytes())
                    .await
                    .map_err(|error| format!("Failed to send handshake response: {error}"))?;
                Ok(IncomingConnection::Plain(
                    crate::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
                        stream,
                        handshake.peer_id,
                    ),
                ))
            }
            Self::Mse {
                mut stream,
                handshake,
                mut crypto,
            } => {
                let mut response = Handshake::new(&handshake.info_hash, &local_peer_id).to_bytes();
                crypto.encrypt(&mut response);
                stream
                    .write_all(&response)
                    .await
                    .map_err(|error| format!("Failed to send MSE handshake response: {error}"))?;
                Ok(IncomingConnection::Encrypted(
                    crate::bittorrent::peer::encrypted_connection::EncryptedConnection::from_incoming_parts(
                        stream,
                        crypto,
                        handshake.peer_id,
                    ),
                ))
            }
        }
    }
}

/// Read and negotiate an incoming connection before route selection.
pub async fn receive(
    stream: TcpStream,
    known_info_hashes: &[[u8; 20]],
) -> Result<IncomingHandshake, String> {
    receive_with_policies(stream, known_info_hashes, &HashMap::new()).await
}

/// Read an incoming handshake using policies indexed by torrent info-hash.
///
/// The info-hash is intentionally concealed until MSE `req2 ^ req3` has been
/// checked. Only then can the route's download options affect negotiation.
pub async fn receive_with_policies(
    mut stream: TcpStream,
    known_info_hashes: &[[u8; 20]],
    policies: &HashMap<[u8; 20], IncomingCryptoPolicy>,
) -> Result<IncomingHandshake, String> {
    let mut prefix = [0u8; 20];
    read_exact(&mut stream, &mut prefix).await?;
    if prefix[0] == 19 && &prefix[1..20] == b"BitTorrent protocol" {
        let mut bytes = [0u8; HANDSHAKE_LENGTH];
        bytes[..20].copy_from_slice(&prefix);
        read_exact(&mut stream, &mut bytes[20..]).await?;
        let handshake = Handshake::parse(&bytes)?;
        if policies
            .get(&handshake.info_hash)
            .is_some_and(|policy| policy.reject_plain)
        {
            return Err("The legacy BitTorrent handshake is disabled by policy".to_string());
        }
        return Ok(IncomingHandshake::Plain {
            stream,
            handshake,
        });
    }

    let mut responder = MseHandshake::new_responder([0u8; 20]);
    let mut public_key = vec![0u8; MSE_PUBLIC_KEY_LENGTH];
    public_key[..20].copy_from_slice(&prefix);
    read_exact(&mut stream, &mut public_key[20..]).await?;
    responder.receive_step1(&public_key)?;
    stream
        .write_all(&responder.build_step1())
        .await
        .map_err(|error| format!("Failed to send MSE public key: {error}"))?;

    let mut step2 = Vec::with_capacity(MSE_MAX_BUFFER_LENGTH);
    loop {
        if let Some(info_hash) = responder.receiver_info_hash(&step2, known_info_hashes)? {
            responder.set_info_hash(info_hash)?;
            break;
        }
        if step2.len() >= MSE_MAX_BUFFER_LENGTH {
            return Err("MSE handshake exceeded receiver buffer limit".to_string());
        }
        let mut byte = [0u8; 1];
        read_exact(&mut stream, &mut byte).await?;
        step2.push(byte[0]);
    }

    let required = responder
        .receiver_step2_required_len(&step2)?
        .ok_or("MSE handshake length is incomplete")?;
    if required > MSE_MAX_BUFFER_LENGTH {
        return Err("MSE handshake IA exceeds receiver buffer limit".to_string());
    }
    while step2.len() < required {
        let remaining = required - step2.len();
        let chunk_len = remaining.min(64);
        let mut chunk = vec![0u8; chunk_len];
        read_exact(&mut stream, &mut chunk).await?;
        step2.extend_from_slice(&chunk);
    }

    let info_hash = *responder.info_hash();
    let policy = policies.get(&info_hash).copied().unwrap_or_default();
    responder.set_crypto_preferences(policy.force_encryption, policy.prefer_encryption);
    responder.receive_initiator_step2(&step2, &[info_hash])?;
    let response = responder.build_receiver_step2()?;
    stream
        .write_all(&response)
        .await
        .map_err(|error| format!("Failed to send MSE response: {error}"))?;
    let mut crypto = responder.finalize()?;

    let mut handshake_bytes = [0u8; HANDSHAKE_LENGTH];
    read_exact(&mut stream, &mut handshake_bytes).await?;
    let mut handshake_data = handshake_bytes.to_vec();
    crypto.decrypt(&mut handshake_data);
    let handshake = Handshake::parse(&handshake_data)?;
    if handshake.info_hash != info_hash {
        return Err("MSE BitTorrent handshake info_hash mismatch".to_string());
    }

    Ok(IncomingHandshake::Mse {
        stream,
        handshake,
        crypto,
    })
}

async fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), String> {
    tokio::time::timeout(READ_TIMEOUT, stream.read_exact(buffer))
        .await
        .map_err(|_| "Incoming handshake read timeout".to_string())?
        .map(|_| ())
        .map_err(|error| format!("Incoming handshake read failed: {error}"))
}
