use std::collections::HashSet;

use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

pub(crate) enum InnerConnection {
    Plain(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Encrypted(aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection),
}

/// Peer connection abstraction that supports both plain and encrypted (MSE) connections.
///
/// This mirrors the original aria2 C++ architecture where connection management
/// is separated from the download command logic (see BtRuntime in original).
pub struct BtPeerConn {
    pub(crate) inner: InnerConnection,
    /// Set of piece indices for which the peer has sent an AllowedFast message.
    /// Pieces in this set can be requested even when the peer is choked.
    pub allowed_fast: HashSet<u32>,
}

impl BtPeerConn {
    pub async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => Ok(Self {
                inner: InnerConnection::Encrypted(conn),
                allowed_fast: HashSet::new(),
            }),
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
            Ok(conn) => Ok(Self {
                inner: InnerConnection::Plain(conn),
                allowed_fast: HashSet::new(),
            }),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Add a piece index to the AllowedFast set.
    ///
    /// Called when an AllowedFast message is received from this peer.
    /// Pieces in the allowed_fast set can be requested even when the peer
    /// is choked (BEP 6 / Fast Extension).
    pub fn add_allowed_fast(&mut self, index: u32) {
        self.allowed_fast.insert(index);
    }

    /// Check whether a piece index is in the AllowedFast set.
    ///
    /// Returns true if the peer has granted fast access to this piece,
    /// meaning a Request can be sent even while the peer is choked.
    pub fn is_allowed_fast(&self, index: u32) -> bool {
        self.allowed_fast.contains(&index)
    }

    /// Get a reference to the full AllowedFast set
    ///
    /// Returns all piece indices that this peer has allowed us to request
    /// via BEP 6 Fast Extension, even when choked.
    pub fn allowed_fast_set(&self) -> &HashSet<u32> {
        &self.allowed_fast
    }

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
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
        }
    }

    pub async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
        }
    }

    pub fn is_connected(&self) -> bool {
        match &self.inner {
            InnerConnection::Plain(c) => c.is_connected(),
            InnerConnection::Encrypted(c) => c.is_connected(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self.inner, InnerConnection::Encrypted(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_fast_set_operations() {
        let mut set: HashSet<u32> = HashSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(&42));
        set.insert(42);
        assert!(set.contains(&42));
        set.insert(10);
        set.insert(99);
        assert_eq!(set.len(), 3);
        assert!(!set.contains(&999));
        set.insert(42);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn test_allowed_fast_multiple_indices() {
        let mut set: HashSet<u32> = HashSet::new();
        for i in 0..100u32 {
            set.insert(i);
        }
        assert_eq!(set.len(), 100);
        for i in 0..100u32 {
            assert!(set.contains(&i));
        }
        assert!(!set.contains(&100));
    }
}
