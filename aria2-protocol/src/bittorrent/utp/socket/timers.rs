use super::*;

impl UtpSocket {
    /// Process timers and handle expired ones
    pub fn process_timers(&mut self) -> Result<(), UtpSocketError> {
        let expired = self.timers.get_expired_timers();

        for (conn_id, timer_type) in expired {
            self.handle_timer_expired(conn_id, timer_type)?;
        }

        Ok(())
    }

    /// Handle an expired timer
    fn handle_timer_expired(
        &mut self,
        conn_id: u16,
        timer_type: TimerType,
    ) -> Result<(), UtpSocketError> {
        match timer_type {
            TimerType::ConnectTimeout => {
                let should_close = {
                    let conn = self.connections.get(&conn_id);
                    conn.is_some_and(|c| c.state() == ConnectionState::SynSent)
                };

                if should_close {
                    self.close_connection_internal(conn_id)?;
                }
            }
            TimerType::Retransmit(seq_nr) => {
                let (is_timeout, remote_addr, packet_to_send) = {
                    let conn = self.connections.get_mut(&conn_id);
                    if let Some(conn) = conn {
                        let is_timeout = conn.check_timeout(self.idle_timeout);
                        let remote_addr = conn.remote_addr();
                        let packet = conn.retransmit_packet(seq_nr);
                        (is_timeout, remote_addr, packet)
                    } else {
                        return Ok(());
                    }
                };

                if is_timeout {
                    self.close_connection_internal(conn_id)?;
                } else if packet_to_send.is_some()
                    && !self
                        .timers
                        .has_timer(conn_id, TimerType::Retransmit(seq_nr))
                {
                    self.close_connection_internal(conn_id)?;
                } else if self
                    .timers
                    .has_timer(conn_id, TimerType::Retransmit(seq_nr))
                {
                    if let Some(packet) = packet_to_send {
                        if let Some(addr) = remote_addr {
                            self.send_packet(&packet, addr)?;
                        }
                    } else {
                        self.timers
                            .cancel_timer(conn_id, TimerType::Retransmit(seq_nr));
                    }
                }
            }
            TimerType::Keepalive => {
                let (is_established, state_packet, remote_addr, keepalive_interval) = {
                    let conn = self.connections.get(&conn_id);
                    if let Some(conn) = conn {
                        let is_established = conn.is_established();
                        let state_packet = conn.state_packet();
                        let remote_addr = conn.remote_addr();
                        let keepalive_interval = self.keepalive_interval;
                        (
                            is_established,
                            state_packet,
                            remote_addr,
                            keepalive_interval,
                        )
                    } else {
                        return Ok(());
                    }
                };

                if is_established {
                    if let Some(addr) = remote_addr {
                        self.send_packet(&state_packet, addr)?;
                    }
                    self.timers
                        .set_timer(conn_id, TimerType::Keepalive, keepalive_interval);
                }
            }
            TimerType::IdleTimeout => {
                self.close_connection_internal(conn_id)?;
            }
        }

        Ok(())
    }
}
