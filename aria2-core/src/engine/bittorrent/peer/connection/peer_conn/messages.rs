//! Protocol message senders, message reading, and connection state queries for
//! [`BtPeerConn`].

use crate::error::{Aria2Error, RecoverableError, Result};
use aria2_protocol::bittorrent::message::types::BtMessage;

use super::{BtPeerConn, InnerConnection};

impl BtPeerConn {
    pub(crate) async fn send_bt_message(&mut self, message: &BtMessage) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(connection) => {
                connection.send_message(message).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })
            }
            InnerConnection::Utp(connection) => connection.send_bt_message(message).await,
        }
    }

    // -----------------------------------------------------------------------
    // Keep-alive send (uses write_raw, kept here with other senders)
    // -----------------------------------------------------------------------

    /// Send a keep-alive message (4-byte zero-length prefix).
    ///
    /// Also updates `last_keepalive_sent`.
    pub async fn send_keepalive(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::types::BtMessage;
        self.send_bt_message(&BtMessage::KeepAlive).await?;
        self.last_keepalive_sent = std::time::Instant::now();
        Ok(())
    }

    /// Send the BEP 10 extension handshake using the task's peer-agent value.
    pub async fn send_extension_handshake(&mut self, peer_agent: &str) -> Result<()> {
        self.send_extension_handshake_with_port(peer_agent, None)
            .await
    }

    /// Send the BEP 10 extension handshake and optionally advertise the local
    /// TCP listen port (`p`), matching aria2's post-handshake setup.
    pub async fn send_extension_handshake_with_port(
        &mut self,
        peer_agent: &str,
        port: Option<u16>,
    ) -> Result<()> {
        self.send_extension_handshake_with_metadata(peer_agent, port, None)
            .await
    }

    pub(crate) async fn send_extension_handshake_with_metadata(
        &mut self,
        peer_agent: &str,
        port: Option<u16>,
        metadata_size: Option<u32>,
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
        use aria2_protocol::bittorrent::message::types::BtMessage;

        let mut handshake = ExtensionHandshake::new();
        handshake.with_version(peer_agent);
        if let Some(port) = port.filter(|port| *port != 0) {
            handshake.with_port(port);
        }
        if let Some(metadata_size) = metadata_size {
            handshake.with_metadata_size(metadata_size);
        }
        self.send_bt_message(&BtMessage::Extended {
            ext_id: 0,
            payload: handshake.to_bytes(),
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Protocol message senders (immediate flush — preserved API)
    // -----------------------------------------------------------------------

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::Unchoke).await,
        }
    }

    pub async fn send_choke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::Choke).await,
        }
    }

    pub async fn send_interested(&mut self) -> Result<()> {
        let result = match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::Interested).await,
        };
        result?;
        self.stats.am_interested = true;
        Ok(())
    }

    pub async fn send_not_interested(&mut self) -> Result<()> {
        let result = match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::NotInterested).await,
        };
        result?;
        self.stats.am_interested = false;
        Ok(())
    }

    /// Send the BEP 5 DHT port message.
    pub async fn send_port(&mut self, port: u16) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c
                .send_message(&aria2_protocol::bittorrent::message::types::BtMessage::Port { port })
                .await
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::Port { port }).await,
        }
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::Have { piece_index }).await,
        }
    }

    /// Send a pre-serialized HAVE frame.
    ///
    /// The frame is shared by the broadcast caller; encrypted transports still
    /// perform their required per-connection encryption copy.
    pub async fn send_have_frame(&mut self, frame: &[u8]) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_serialized(frame).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_message(frame).await,
        }
    }

    pub async fn send_request(
        &mut self,
        req: aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                c.send_bt_message(&BtMessage::Request { request: req })
                    .await
            }
        }
    }

    pub async fn send_cancel(
        &mut self,
        req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                c.send_bt_message(&BtMessage::Cancel {
                    request: req.clone(),
                })
                .await
            }
        }
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                c.send_bt_message(&BtMessage::Bitfield { data: bitfield })
                    .await
            }
        }
    }

    /// Send a HaveAll message (BEP 6 Fast Extension).
    /// Indicates that the sender has all pieces.
    pub async fn send_have_all(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::types::BtMessage;
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_message(&BtMessage::HaveAll).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::HaveAll).await,
        }
    }

    /// Send a HaveNone message (BEP 6 Fast Extension).
    /// Indicates that the sender has no pieces.
    pub async fn send_have_none(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::types::BtMessage;
        match &mut self.inner {
            InnerConnection::Tcp(c) => c.send_message(&BtMessage::HaveNone).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_bt_message(&BtMessage::HaveNone).await,
        }
    }

    // -----------------------------------------------------------------------
    // Message reading
    // -----------------------------------------------------------------------

    pub async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        self.read_message_validated(None).await
    }

    /// Read and optionally apply torrent-domain validation before dispatch.
    pub async fn read_message_validated(
        &mut self,
        validator: Option<&crate::engine::bittorrent::peer::message_validation::BtMessageValidator>,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        let result = match tokio::time::timeout(self.peer_timeout, async {
            match &mut self.inner {
                InnerConnection::Tcp(c) => c.read_message().await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                }),
                InnerConnection::Utp(c) => c.recv_bt_message().await,
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "peer inactivity timeout after {} seconds",
                        self.peer_timeout.as_secs()
                    ),
                },
            )),
        };
        let result = result.and_then(|message| {
            if let Some(message_ref) = message.as_ref() {
                if let Err(error) =
                    aria2_protocol::bittorrent::peer::message_validation::validate_message_capabilities(
                        message_ref,
                        self.remote_supports_fast_extension(),
                        self.remote_supports_extended_messaging(),
                    )
                {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: error.to_string(),
                        },
                    ));
                }
                let validation = if let Some(validator) = validator {
                    validator.validate(message_ref)
                } else if self.metadata_pending {
                    Ok(())
                } else if let Some(resource) = &self.session_resource {
                    resource.validate_message(message_ref)
                } else {
                    Ok(())
                };
                validation.map_err(invalid_peer_message_error)?;
            }

            if let Some(aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                ext_id: 0,
                payload,
            }) = message.as_ref()
                && let Ok(handshake) =
                    aria2_protocol::bittorrent::message::extension::ExtensionHandshake::from_bytes(
                        payload,
                    )
            {
                if let Some(resource) = self.session_resource.as_mut() {
                    resource.apply_extension_handshake(&handshake);
                }
                if let Some(port) = handshake.port() {
                    self.remote_listen_port = Some(port);
                    self.incoming = false;
                }
                if let Some(client) = handshake
                    .v()
                    .map(str::trim)
                    .filter(|client| !client.is_empty())
                {
                    *self
                        .remote_client
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(client.to_owned());
                }
            }
            Ok(message)
        });
        // Update keep-alive tracking on any successfully validated message receipt
        if result.is_ok() {
            self.on_message_received();
        }
        if let Ok(Some(aria2_protocol::bittorrent::message::types::BtMessage::Piece {
            data, ..
        })) = &result
        {
            self.stats.on_data_received(data.len() as u64);
            if let Some(progress) = self.upload_progress.as_ref() {
                progress.record_download_payload(data.len() as u64);
            }
        }
        result
    }

    // -----------------------------------------------------------------------
    // Connection state queries
    // -----------------------------------------------------------------------

    pub fn is_connected(&self) -> bool {
        match &self.inner {
            InnerConnection::Tcp(connection) => connection.is_connected(),
            InnerConnection::Utp(connection) => connection.is_connected(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(&self.inner, InnerConnection::Tcp(connection) if connection.is_encrypted())
    }

    // -----------------------------------------------------------------------
    // Serialized frame output
    // -----------------------------------------------------------------------

    pub(crate) async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Tcp(connection) => {
                connection.send_serialized(data).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })
            }
            InnerConnection::Utp(connection) => connection.send_message(data).await,
        }
    }
}

fn invalid_peer_message_error(error: impl std::fmt::Display) -> Aria2Error {
    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
        message: format!("invalid BitTorrent peer message: {error}"),
    })
}

#[async_trait::async_trait]
impl crate::engine::bittorrent::peer::upload_session::BtUploadTransport for BtPeerConn {
    fn supports_fast_extension(&self) -> bool {
        self.remote_supports_fast_extension()
    }

    fn am_allowed_fast(&self, piece_index: u32) -> bool {
        BtPeerConn::am_allowed_fast(self, piece_index)
    }

    async fn send_upload_message(
        &mut self,
        message: &BtMessage,
    ) -> std::result::Result<(), String> {
        let result = self.send_bt_message(message).await;
        if result.is_ok() {
            self.record_outbound_activity();
        }
        result.map_err(|error| error.to_string())
    }

    async fn send_upload_choke(&mut self) -> std::result::Result<(), String> {
        let result = self.send_choke().await;
        if result.is_ok() {
            self.record_outbound_activity();
        }
        result.map_err(|error| error.to_string())
    }

    async fn send_upload_unchoke(&mut self) -> std::result::Result<(), String> {
        let result = self.send_unchoke().await;
        if result.is_ok() {
            self.record_outbound_activity();
        }
        result.map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod validation_error_tests {
    use super::*;

    #[test]
    fn invalid_peer_geometry_is_a_recoverable_network_failure() {
        let error = invalid_peer_message_error(
            aria2_protocol::bittorrent::peer::message_validation::BtMessageValidationError::IndexOutOfRange {
                index: 2,
                num_pieces: 1,
            },
        );

        assert!(matches!(
            error,
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message })
                if message.contains("invalid BitTorrent peer message")
        ));
    }
}
