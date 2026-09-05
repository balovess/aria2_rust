//! Incoming BitTorrent handshake adapter.
//!
//! This is the protocol seam used by the process-level listener. It hides the
//! difference between a legacy 68-byte handshake and aria2-compatible MSE,
//! while leaving route selection to the caller through `info_hash()`.

use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::extension::mse_handshake::{
    MSE_MAX_INCOMING_HANDSHAKE_LENGTH, MSE_PUBLIC_KEY_LENGTH, MseHandshake,
};
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
    Plain(Box<crate::bittorrent::peer::connection::PeerConnection>),
    Encrypted(Box<crate::bittorrent::peer::encrypted_connection::EncryptedConnection>),
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
        crypto: Box<MseCryptoState>,
    },
}

impl IncomingHandshake {
    pub fn info_hash(&self) -> &[u8; 20] {
        match self {
            Self::Plain { handshake, .. } | Self::Mse { handshake, .. } => &handshake.info_hash,
        }
    }

    pub async fn complete(self, local_peer_id: [u8; 20]) -> Result<IncomingConnection, String> {
        self.complete_with_hybrid(local_peer_id, None).await
    }

    /// Complete the handshake and upgrade a hybrid responder to the v2
    /// truncated hash when the remote advertised BEP 52.
    pub async fn complete_with_hybrid(
        self,
        local_peer_id: [u8; 20],
        info_hash_v2: Option<[u8; 32]>,
    ) -> Result<IncomingConnection, String> {
        match self {
            Self::Plain {
                mut stream,
                handshake,
            } => {
                let response_hash = info_hash_v2
                    .filter(|_| handshake.supports_bep52())
                    .map(|hash| hash[..20].try_into().expect("SHA-256 hash is 32 bytes"))
                    .unwrap_or(handshake.info_hash);
                stream
                    .write_all(
                        &Handshake::new(&response_hash, &local_peer_id)
                            .with_bep52(info_hash_v2.is_some())
                            .to_bytes(),
                    )
                    .await
                    .map_err(|error| format!("Failed to send handshake response: {error}"))?;
                Ok(IncomingConnection::Plain(Box::new(
                    crate::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
                        stream,
                        handshake.peer_id,
                    ),
                )))
            }
            Self::Mse {
                mut stream,
                handshake,
                mut crypto,
            } => {
                let response_hash = info_hash_v2
                    .filter(|_| handshake.supports_bep52())
                    .map(|hash| hash[..20].try_into().expect("SHA-256 hash is 32 bytes"))
                    .unwrap_or(handshake.info_hash);
                let mut response = Handshake::new(&response_hash, &local_peer_id)
                    .with_bep52(info_hash_v2.is_some())
                    .to_bytes();
                crypto.encrypt(&mut response);
                stream
                    .write_all(&response)
                    .await
                    .map_err(|error| format!("Failed to send MSE handshake response: {error}"))?;
                Ok(IncomingConnection::Encrypted(
                    Box::new(crate::bittorrent::peer::encrypted_connection::EncryptedConnection::from_incoming_parts(
                        stream,
                        *crypto,
                        handshake.peer_id,
                    )),
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
        return Ok(IncomingHandshake::Plain { stream, handshake });
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

    let mut step2 = Vec::with_capacity(MSE_MAX_INCOMING_HANDSHAKE_LENGTH);
    let info_hash = loop {
        if let Some(info_hash) = responder.receiver_info_hash(&step2, known_info_hashes)? {
            responder.set_info_hash(info_hash)?;
            break info_hash;
        }
        if step2.len() >= MSE_MAX_INCOMING_HANDSHAKE_LENGTH {
            return Err(format!(
                "MSE handshake exceeded receiver buffer limit; prefix={:02x?}",
                &step2[..step2.len().min(48)]
            ));
        }
        let mut byte = [0u8; 1];
        read_exact(&mut stream, &mut byte).await?;
        step2.push(byte[0]);
    };

    let required = loop {
        if let Some(required) = responder.receiver_step2_required_len(&step2)? {
            break required;
        }
        if step2.len() >= MSE_MAX_INCOMING_HANDSHAKE_LENGTH {
            return Err(format!(
                "MSE handshake exceeded receiver buffer limit; prefix={:02x?}",
                &step2[..step2.len().min(48)]
            ));
        }
        let mut byte = [0u8; 1];
        read_exact(&mut stream, &mut byte).await?;
        step2.push(byte[0]);
    };
    if required > MSE_MAX_INCOMING_HANDSHAKE_LENGTH {
        return Err("MSE handshake IA exceeds receiver buffer limit".to_string());
    }
    while step2.len() < required {
        let remaining = required - step2.len();
        let chunk_len = remaining.min(64);
        let mut chunk = vec![0u8; chunk_len];
        read_exact(&mut stream, &mut chunk).await?;
        step2.extend_from_slice(&chunk);
    }

    debug_assert_eq!(info_hash, *responder.info_hash());
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
    let handshake = Handshake::parse(&handshake_data).map_err(|error| {
        format!(
            "{error}; decrypted handshake prefix={:02x?}",
            &handshake_data[..8]
        )
    })?;
    if handshake.info_hash != info_hash {
        return Err("MSE BitTorrent handshake info_hash mismatch".to_string());
    }

    Ok(IncomingHandshake::Mse {
        stream,
        handshake,
        crypto: Box::new(crypto),
    })
}

async fn read_exact(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<(), String> {
    tokio::time::timeout(READ_TIMEOUT, stream.read_exact(buffer))
        .await
        .map_err(|_| "Incoming handshake read timeout".to_string())?
        .map(|_| ())
        .map_err(|error| format!("Incoming handshake read failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::message::types::BtMessage;
    use crate::bittorrent::peer::connection::PeerAddr;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn run_incoming_server(
        info_hash: [u8; 20],
        policy: IncomingCryptoPolicy,
    ) -> (
        SocketAddr,
        tokio::task::JoinHandle<Result<IncomingConnection, String>>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
            let mut policies = HashMap::new();
            policies.insert(info_hash, policy);
            let incoming = receive_with_policies(stream, &[info_hash], &policies).await?;
            incoming.complete([2u8; 20]).await
        });
        (address, server)
    }

    #[tokio::test]
    async fn mse_rc4_socket_handshake_preserves_post_handshake_cipher_state() {
        let info_hash = [0x31u8; 20];
        let (address, server) = run_incoming_server(
            info_hash,
            IncomingCryptoPolicy {
                reject_plain: true,
                force_encryption: true,
                prefer_encryption: true,
            },
        )
        .await;

        let client_result =
            crate::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(
                &PeerAddr::new("127.0.0.1", address.port()),
                &info_hash,
                true,
                true,
            )
            .await;
        let mut client = match client_result {
            Ok(client) => client,
            Err(error) => {
                let server_result = server.await.unwrap();
                let server_error = match server_result {
                    Ok(_) => "ok".to_owned(),
                    Err(server_error) => server_error,
                };
                panic!("client: {error}; server: {server_error}");
            }
        };
        assert!(client.is_encrypted());
        client.send_message(&BtMessage::KeepAlive).await.unwrap();
        let mut server_connection = server.await.unwrap().unwrap();
        let server_connection = match &mut server_connection {
            IncomingConnection::Encrypted(connection) => connection,
            IncomingConnection::Plain(_) => panic!("expected an encrypted connection"),
        };
        assert_eq!(
            server_connection.read_message().await.unwrap(),
            Some(BtMessage::KeepAlive)
        );

        server_connection
            .send_message(&BtMessage::Choke)
            .await
            .unwrap();
        assert_eq!(client.read_message().await.unwrap(), Some(BtMessage::Choke));
    }

    #[tokio::test]
    async fn mse_plain_socket_handshake_keeps_aria2_fallback() {
        let info_hash = [0x32u8; 20];
        let (address, server) =
            run_incoming_server(info_hash, IncomingCryptoPolicy::default()).await;

        let client =
            crate::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(
                &PeerAddr::new("127.0.0.1", address.port()),
                &info_hash,
                false,
                false,
            )
            .await
            .unwrap();
        assert!(!client.is_encrypted());
        let server_connection = server.await.unwrap().unwrap();
        assert!(matches!(
            server_connection,
            IncomingConnection::Encrypted(_)
        ));
    }

    #[tokio::test]
    async fn legacy_plain_handshake_is_rejected_by_policy() {
        let info_hash = [0x33u8; 20];
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut policies = HashMap::new();
            policies.insert(
                info_hash,
                IncomingCryptoPolicy {
                    reject_plain: true,
                    ..IncomingCryptoPolicy::default()
                },
            );
            receive_with_policies(stream, &[info_hash], &policies).await
        });

        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(&Handshake::new(&info_hash, &[9u8; 20]).to_bytes())
            .await
            .unwrap();
        let result = server.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mse_hybrid_handshake_upgrades_to_v2_hash() {
        let v1 = [0x41u8; 20];
        let v2 = [0x42u8; 32];
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut policies = HashMap::new();
            policies.insert(
                v1,
                IncomingCryptoPolicy {
                    reject_plain: true,
                    force_encryption: true,
                    prefer_encryption: true,
                },
            );
            let incoming = receive_with_policies(stream, &[v1], &policies).await?;
            incoming.complete_with_hybrid([0x44; 20], Some(v2)).await
        });

        let connection = crate::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse_hybrid_with_options(
            &PeerAddr::new("127.0.0.1", address.port()),
            &v1,
            &v2,
            true,
            true,
            &[0x43; 20],
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(connection.is_encrypted());
        let server_connection = server.await.unwrap().unwrap();
        assert_eq!(server_connection.remote_peer_id(), Some([0x43; 20]));
    }
}
