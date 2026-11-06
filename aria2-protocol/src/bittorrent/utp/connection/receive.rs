use super::*;

const MAX_PENDING_PACKETS: usize = 1024;
const MAX_SELECTIVE_ACK_BYTES: usize = 252;

impl UtpConnection {
    pub(super) fn handle_data_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<PacketHandling, ConnectionError> {
        self.receive_sequenced(packet)
    }

    pub(super) fn handle_fin_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<PacketHandling, ConnectionError> {
        self.receive_sequenced(packet)
    }

    fn receive_sequenced(&mut self, packet: &UtpPacket) -> Result<PacketHandling, ConnectionError> {
        if !matches!(
            self.state,
            ConnectionState::Established | ConnectionState::FinWait | ConnectionState::Closing
        ) {
            return Err(ConnectionError::NotConnected);
        }
        let extensions = packet.parse_extensions()?;
        let (acknowledged, fast_retransmit) =
            self.acknowledge_sent(packet, extensions.selective_ack)?;
        let distance = packet.seq_nr.wrapping_sub(self.ack_nr);
        let is_fin = packet.packet_type().ok() == Some(PacketType::StFin);
        if self.state != ConnectionState::Closing
            && (!self.remote_fin_received || is_fin)
            && distance > 0
            && distance < 0x8000
        {
            let free = RECV_BUFFER_SIZE
                .saturating_sub(self.recv_buffer.len() + self.pending_receive_bytes);
            if distance == 1 && extensions.application_payload.len() <= free {
                self.deliver_packet(packet, extensions.application_payload);
                while self.state != ConnectionState::Closing
                    && self.state != ConnectionState::Closed
                {
                    let next = self.ack_nr.wrapping_add(1);
                    let Some(packet) = self.pending_receive.remove(&next) else {
                        break;
                    };
                    self.pending_receive_bytes -= packet.payload.len();
                    self.deliver_packet(&packet, &packet.payload);
                }
            } else if distance > 1
                && self.pending_receive.len() < MAX_PENDING_PACKETS
                && extensions.application_payload.len() <= free
                && self.pending_receive_bytes + extensions.application_payload.len()
                    <= RECV_BUFFER_SIZE / 2
                && !self.pending_receive.contains_key(&packet.seq_nr)
            {
                self.pending_receive_bytes += extensions.application_payload.len();
                let mut packet = packet.clone();
                packet.extension = 0;
                packet.payload = extensions.application_payload.to_vec();
                self.pending_receive.insert(packet.seq_nr, packet);
            }
        }
        self.update_receive_window();
        Ok(PacketHandling {
            response_packets: vec![self.state_packet()],
            acknowledged,
            fast_retransmit,
        })
    }

    fn deliver_packet(&mut self, packet: &UtpPacket, payload: &[u8]) {
        self.ack_nr = packet.seq_nr;
        if packet.packet_type().ok() == Some(PacketType::StFin) {
            self.remote_fin_received = true;
            self.state = if self.local_fin_acked {
                ConnectionState::Closed
            } else if self.state == ConnectionState::FinWait {
                ConnectionState::FinWait
            } else {
                ConnectionState::Closing
            };
            self.pending_receive.clear();
            self.pending_receive_bytes = 0;
        } else {
            self.recv_buffer.extend_from_slice(payload);
        }
    }

    pub(super) fn add_selective_ack(&self, packet: &mut UtpPacket) {
        let Some(mask) = self.selective_ack_mask() else {
            return;
        };
        packet.set_selective_ack(&mask);
    }

    fn selective_ack_mask(&self) -> Option<Vec<u8>> {
        let highest_bit = self
            .pending_receive
            .keys()
            .filter_map(|sequence| {
                let distance = sequence.wrapping_sub(self.ack_nr);
                if distance > 1 && distance < 0x8000 {
                    Some(usize::from(distance) - 2)
                } else {
                    None
                }
            })
            .filter(|bit| *bit < MAX_SELECTIVE_ACK_BYTES * 8)
            .max()?;
        let mask_length = (highest_bit / 8 + 1).div_ceil(4) * 4;
        let mut mask = vec![0; mask_length.min(MAX_SELECTIVE_ACK_BYTES)];

        for sequence in self.pending_receive.keys() {
            let distance = sequence.wrapping_sub(self.ack_nr);
            if distance <= 1 || distance >= 0x8000 {
                continue;
            }
            let bit = usize::from(distance) - 2;
            if bit < mask.len() * 8 {
                mask[bit / 8] |= 1 << (bit % 8);
            }
        }

        Some(mask)
    }

    pub(super) fn update_receive_window(&mut self) {
        self.recv_window = RECV_BUFFER_SIZE
            .saturating_sub(self.recv_buffer.len() + self.pending_receive_bytes)
            as u32;
    }
}
