use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

/// Peer connection abstraction that supports both plain and encrypted (MSE) connections.
///
/// This mirrors the original aria2 C++ architecture where connection management
/// is separated from the download command logic (see BtRuntime in original).
pub enum BtPeerConn {
    Plain(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Encrypted(aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection),
}

impl BtPeerConn {
    pub async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => Ok(BtPeerConn::Encrypted(conn)),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    pub async fn connect_plain(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash)
            .await
        {
            Ok(conn) => Ok(BtPeerConn::Plain(conn)),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_choke(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_not_interested(&mut self) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_request(
        &mut self,
        req: aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_cancel(
        &mut self,
        req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match self {
            BtPeerConn::Plain(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        match self {
            BtPeerConn::Plain(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            BtPeerConn::Encrypted(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub fn is_connected(&self) -> bool {
        match self {
            BtPeerConn::Plain(c) => c.is_connected(),
            BtPeerConn::Encrypted(c) => c.is_connected(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, BtPeerConn::Encrypted(_))
    }
}
