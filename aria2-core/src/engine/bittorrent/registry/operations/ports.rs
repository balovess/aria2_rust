//! BitTorrent registry ports operations.

use super::super::BtRegistry;
use tracing::trace;

impl BtRegistry {
    // -----------------------------------------------------------------------
    // Port configuration
    // -----------------------------------------------------------------------

    /// Set the TCP listen port for incoming BitTorrent connections.
    pub fn set_tcp_port(&mut self, port: u16) {
        trace!(port, "BtRegistry::set_tcp_port");
        self.tcp_port = port;
    }

    /// Get the TCP listen port.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// Set the UDP port for DHT and UDP tracker.
    pub fn set_udp_port(&mut self, port: u16) {
        trace!(port, "BtRegistry::set_udp_port");
        self.udp_port = port;
    }

    /// Get the UDP port.
    pub fn udp_port(&self) -> u16 {
        self.udp_port
    }

    // -----------------------------------------------------------------------
}
