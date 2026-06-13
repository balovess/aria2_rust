//! uTP (Micro Transport Protocol) implementation
//!
//! This module implements BEP 29 - uTorrent transport protocol.
//! uTP is a transport protocol layered on top of UDP that provides
//! reliable, ordered delivery with congestion control.

pub mod connection;
pub mod congestion;
pub mod metrics;
pub mod packet;
pub mod socket;
pub mod timer;

pub use connection::{ConnectionError, ConnectionState, UtpConnection};
pub use congestion::{
    GAIN, LEDBAT_MAX_CWND, LEDBAT_MIN_CWND, LEDBAT_TARGET_DELAY, LedbatController,
};
pub use metrics::{BandwidthEstimator, DelayEstimator, RttEstimator};
pub use packet::{PacketType, UtpPacket, UtpPacketError, UTP_HEADER_SIZE, UTP_VERSION};
pub use socket::{AsyncUtpSocket, ConnectionStats, ConnectionId, UtpSocket, UtpSocketError};
pub use timer::{
    IdleTimeoutDetector, KeepaliveManager, RetransmitScheduler, TimerManager, TimerType,
};