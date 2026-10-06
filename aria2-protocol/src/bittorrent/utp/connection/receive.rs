use super::*;

const MAX_PENDING_PACKETS: usize = 1024;

impl UtpConnection {
    pub(super) fn handle_data_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<(Vec<UtpPacket>, Vec<u16>), ConnectionError> {
        self.receive_sequenced(packet)
    }

    pub(super) fn handle_fin_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<(Vec<UtpPacket>, Vec<u16>), ConnectionError> {
        self.receive_sequenced(packet)
    }

    fn receive_sequenced(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<(Vec<UtpPacket>, Vec<u16>), ConnectionError> {
        if !matches!(
            self.state,
            ConnectionState::Established | ConnectionState::FinWait | ConnectionState::Closing
        ) {
            return Err(ConnectionError::NotConnected);
        }
        let acknowledged = self.acknowledge_sent(packet);
        let distance = packet.seq_nr.wrapping_sub(self.ack_nr);
        if self.state != ConnectionState::Closing && distance > 0 && distance < 0x8000 {
            let free = RECV_BUFFER_SIZE
                .saturating_sub(self.recv_buffer.len() + self.pending_receive_bytes);
            if distance == 1 && packet.payload.len() <= free {
                self.deliver_packet(packet);
                while self.state != ConnectionState::Closing
                    && self.state != ConnectionState::Closed
                {
                    let next = self.ack_nr.wrapping_add(1);
                    let Some(packet) = self.pending_receive.remove(&next) else {
                        break;
                    };
                    self.pending_receive_bytes -= packet.payload.len();
                    self.deliver_packet(&packet);
                }
            } else if distance > 1
                && self.pending_receive.len() < MAX_PENDING_PACKETS
                && packet.payload.len() <= free
                && self.pending_receive_bytes + packet.payload.len() <= RECV_BUFFER_SIZE / 2
                && !self.pending_receive.contains_key(&packet.seq_nr)
            {
                self.pending_receive_bytes += packet.payload.len();
                self.pending_receive.insert(packet.seq_nr, packet.clone());
            }
        }
        self.update_receive_window();
        Ok((
            vec![UtpPacket::ack(
                self.remote_conn_id,
                self.ack_nr,
                self.seq_nr,
                self.recv_window,
            )],
            acknowledged,
        ))
    }

    fn deliver_packet(&mut self, packet: &UtpPacket) {
        self.ack_nr = packet.seq_nr;
        if packet.packet_type().ok() == Some(PacketType::StFin) {
            self.state = if self.state == ConnectionState::FinWait {
                ConnectionState::Closed
            } else {
                ConnectionState::Closing
            };
            self.pending_receive.clear();
            self.pending_receive_bytes = 0;
        } else {
            self.recv_buffer.extend_from_slice(&packet.payload);
        }
    }

    pub(super) fn update_receive_window(&mut self) {
        self.recv_window = RECV_BUFFER_SIZE
            .saturating_sub(self.recv_buffer.len() + self.pending_receive_bytes)
            as u32;
    }
}
