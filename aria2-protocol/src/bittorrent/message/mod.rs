/// Maximum length of one length-prefixed BitTorrent message body.
///
/// The limit protects the peer reader from allocating unbounded buffers for a
/// malicious four-byte length prefix. It is large enough for normal bitfields
/// and extension metadata, while individual Piece blocks are limited much
/// lower by core-layer torrent validation.
pub const MAX_BT_MESSAGE_LENGTH: usize = 64 * 1024 * 1024;

pub mod extension;
pub mod factory;
pub mod handshake;
pub mod serializer;
pub mod types;
