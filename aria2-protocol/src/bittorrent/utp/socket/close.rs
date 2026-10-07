//! Graceful uTP connection and socket shutdown.

use super::*;

impl UtpSocket {
    /// Initiate graceful connection close by sending FIN.
    ///
    /// The connection remains available for retransmission and ACK processing;
    /// it is removed after the FIN is acknowledged or the connection times out.
    pub fn close_connection(&mut self, conn_id: u16) -> Result<(), UtpSocketError> {
        if self.is_closed {
            return Err(UtpSocketError::SocketClosed);
        }
        let state = match self.connections.get(&conn_id) {
            Some(conn) => conn.state(),
            None => return Ok(()),
        };
        if state == ConnectionState::FinWait {
            return Ok(());
        }
        if !matches!(
            state,
            ConnectionState::Established | ConnectionState::Closing
        ) {
            self.close_connection_internal(conn_id)?;
            return Ok(());
        }

        let (fin, remote_addr, rto) = {
            let conn = self
                .connections
                .get_mut(&conn_id)
                .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;
            let remote_addr = conn
                .remote_addr()
                .ok_or(UtpSocketError::AddressNotFound(conn_id))?;
            (conn.close()?, remote_addr, conn.rto())
        };

        self.timers.cancel_timer(conn_id, TimerType::ConnectTimeout);
        self.timers.cancel_timer(conn_id, TimerType::Keepalive);
        self.timers.cancel_timer(conn_id, TimerType::IdleTimeout);
        if let Err(error) = self.send_packet(&fin, remote_addr) {
            self.close_connection_internal(conn_id)?;
            return Err(error);
        }
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.record_packet_sent(fin.seq_nr);
        }
        self.timers
            .set_timer(conn_id, TimerType::Retransmit(fin.seq_nr), rto);
        Ok(())
    }

    /// Internal connection close logic
    pub(super) fn close_connection_internal(&mut self, conn_id: u16) -> Result<(), UtpSocketError> {
        let (should_send_fin, remote_addr) = {
            let conn = self.connections.get(&conn_id);
            if let Some(conn) = conn {
                (conn.is_established(), conn.remote_addr())
            } else {
                return Ok(());
            }
        };

        if should_send_fin {
            let fin = {
                let conn = self.connections.get_mut(&conn_id);
                if let Some(conn) = conn {
                    conn.close()?
                } else {
                    return Ok(());
                }
            };

            if let Some(addr) = remote_addr {
                self.send_packet(&fin, addr)?;
            }
        }

        self.timers.cancel_all_timers(conn_id);
        self.connections.remove(&conn_id);

        Ok(())
    }

    /// Close the entire socket and all connections
    pub fn close(&mut self) {
        if self.is_closed {
            return;
        }

        self.is_closed = true;

        let conn_ids: Vec<u16> = self.connections.keys().copied().collect();
        for conn_id in conn_ids {
            let _ = self.close_connection_internal(conn_id);
        }

        self.connections.clear();
        self.timers.clear();
    }
}
