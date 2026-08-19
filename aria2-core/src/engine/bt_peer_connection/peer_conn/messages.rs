//! Protocol message senders, message reading, connection state queries,
//! and low-level write helpers for [`BtPeerConn`].

use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

use super::{BtPeerConn, InnerConnection};

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Keep-alive send (uses write_raw, kept here with other senders)
    // -----------------------------------------------------------------------

    /// Send a keep-alive message (4-byte zero-length prefix).
    ///
    /// Also updates `last_keepalive_sent`.
    pub async fn send_keepalive(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let data = serialize(&BtMessage::KeepAlive);
        self.write_raw(&data).await?;
        self.last_keepalive_sent = std::time::Instant::now();
        Ok(())
    }

    /// Send the BEP 10 extension handshake using the task's peer-agent value.
    pub async fn send_extension_handshake(&mut self, peer_agent: &str) -> Result<()> {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;

        let mut handshake = ExtensionHandshake::new();
        handshake.with_version(peer_agent);
        self.write_raw(&serialize(&BtMessage::Extended {
            ext_id: 0,
            payload: handshake.to_bytes(),
        }))
        .await
    }

    // -----------------------------------------------------------------------
    // Protocol message senders (immediate flush — preserved API)
    // -----------------------------------------------------------------------

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Unchoke;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_choke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Choke;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_interested(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Interested;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_not_interested(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::NotInterested;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Have { piece_index };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    /// Send a pre-serialized HAVE frame.
    ///
    /// The frame is shared by the broadcast caller; encrypted transports still
    /// perform their required per-connection encryption copy.
    pub async fn send_have_frame(&mut self, frame: &[u8]) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_serialized(frame).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_serialized(frame).await.map_err(|e| {
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
            InnerConnection::Plain(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
                let msg = BtMessage::Request {
                    request: PieceBlockRequest::new(req.index, req.begin, req.length),
                };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_cancel(
        &mut self,
        req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
                let msg = BtMessage::Cancel {
                    request: PieceBlockRequest::new(req.index, req.begin, req.length),
                };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Bitfield { data: bitfield };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    /// Send a HaveAll message (BEP 6 Fast Extension).
    /// Indicates that the sender has all pieces.
    pub async fn send_have_all(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let msg = BtMessage::HaveAll;
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_message(&serialize(&msg)).await,
        }
    }

    /// Send a HaveNone message (BEP 6 Fast Extension).
    /// Indicates that the sender has no pieces.
    pub async fn send_have_none(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let msg = BtMessage::HaveNone;
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_message(&serialize(&msg)).await,
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
        validator: Option<&aria2_protocol::bittorrent::message::validation::BtMessageValidator>,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        let result = match tokio::time::timeout(self.peer_timeout, async {
            match &mut self.inner {
                InnerConnection::Plain(c) => c.read_message().await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                }),
                InnerConnection::Encrypted(c) => c.read_message().await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                }),
                InnerConnection::Utp(c) => {
                    let msg_bytes = c.recv_message().await?;
                    if let Some(bytes) = msg_bytes {
                        use aria2_protocol::bittorrent::message::factory::parse_message;
                        parse_message(&bytes).map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))
                    } else {
                        Ok(None)
                    }
                }
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
            if let (Some(validator), Some(message)) = (validator, message.as_ref()) {
                validator.validate(message).map_err(|error| {
                    Aria2Error::Fatal(FatalError::Config(format!(
                        "invalid BitTorrent peer message: {error}"
                    )))
                })?;
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
                if let Some(id) = handshake.ut_metadata_id() {
                    self.register_peer_extension("ut_metadata", id);
                }
                if let Some(id) = handshake.ut_pex_id() {
                    self.register_peer_extension("ut_pex", id);
                }
                if let Some(resource) = &mut self.session_resource {
                    resource.set_extended_messaging_enabled(true);
                }
            }
            Ok(message)
        });
        // Update keep-alive tracking on any successfully validated message receipt
        if result.is_ok() {
            self.on_message_received();
        }
        result
    }

    // -----------------------------------------------------------------------
    // Connection state queries
    // -----------------------------------------------------------------------

    pub fn is_connected(&self) -> bool {
        match &self.inner {
            InnerConnection::Plain(c) => c.is_connected(),
            InnerConnection::Encrypted(c) => c.is_connected(),
            InnerConnection::Utp(c) => c.is_connected(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self.inner, InnerConnection::Encrypted(_))
    }

    // -----------------------------------------------------------------------
    // Low-level write helper
    // -----------------------------------------------------------------------

    /// Write raw bytes directly to the inner connection.
    ///
    /// This is the single point of actual socket write used by both the
    /// immediate-flush `send_*` methods and the `flush_send_buffer` path.
    pub(crate) async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => {
                // PeerConnection exposes send_message(&BtMessage) as the
                // high-level path. For the flush path we split the buffer
                // into individual BT messages and send each one.  This is
                // acceptable because flush is called infrequently.
                Self::flush_raw_to_plain(c, data).await
            }
            InnerConnection::Encrypted(c) => Self::flush_raw_to_encrypted(c, data).await,
            InnerConnection::Utp(c) => c.send_message(data).await,
        }
    }

    /// Flush raw bytes through a plain TCP connection by splitting them
    /// into individual BT messages.
    pub(crate) async fn flush_raw_to_plain(
        conn: &mut aria2_protocol::bittorrent::peer::connection::PeerConnection,
        data: &[u8],
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::factory::parse_message_stream;
        let messages = parse_message_stream(data);
        for (msg, _size) in messages {
            if let Some(bt_msg) = msg {
                conn.send_message(&bt_msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })?;
            }
        }
        Ok(())
    }

    /// Flush raw bytes through an encrypted connection.
    pub(crate) async fn flush_raw_to_encrypted(
        conn: &mut aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection,
        data: &[u8],
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::factory::parse_message_stream;
        let messages = parse_message_stream(data);
        for (msg, _size) in messages {
            if let Some(bt_msg) = msg {
                conn.send_message(&bt_msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })?;
            }
        }
        Ok(())
    }
}
