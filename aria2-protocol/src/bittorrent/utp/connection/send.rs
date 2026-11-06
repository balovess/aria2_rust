//! uTP outbound packet creation, retransmission, and acknowledgment handling.

use super::*;

impl UtpConnection {
    /// Initiate a connection (client-side) - creates SYN packet
    pub fn connect(&mut self, remote_addr: SocketAddr) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::AlreadyExists);
        }

        self.remote_addr = Some(remote_addr);
        self.local_conn_id = super::rand_connection_id();
        self.remote_conn_id = self.local_conn_id.wrapping_add(1);
        self.seq_nr = 1;
        self.syn_seq_nr = self.seq_nr;
        let sent_at = Instant::now();
        self.syn_sent_at = Some(sent_at);
        self.syn_retransmitted = false;
        self.state = ConnectionState::SynSent;

        let mut syn = UtpPacket::syn(self.local_conn_id, self.seq_nr, 0, self.recv_window);
        self.stamp_packet(&mut syn);
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn)
    }

    pub(crate) fn retransmit_packet(
        &mut self,
        seq_nr: u16,
        congestion_loss: bool,
    ) -> Option<UtpPacket> {
        if self.state == ConnectionState::SynSent && seq_nr == self.syn_seq_nr {
            self.syn_retransmitted = true;
            let mut packet =
                UtpPacket::syn(self.local_conn_id, self.syn_seq_nr, 0, self.recv_window);
            self.stamp_packet(&mut packet);
            return Some(packet);
        }

        let packet = self
            .send_buffer
            .iter_mut()
            .find(|sent| sent.packet.seq_nr == seq_nr && !sent.selectively_acked)
            .map(|sent| {
                sent.retransmitted = true;
                let mut packet = sent.packet.clone();
                packet.ack_nr = self.ack_nr;
                packet.wnd_size = self.recv_window;
                packet
            });
        if let Some(mut packet) = packet {
            self.stamp_packet(&mut packet);
            if congestion_loss && packet.packet_type().ok() == Some(PacketType::StData) {
                self.congestion.on_loss();
            }
            Some(packet)
        } else {
            None
        }
    }

    pub(crate) fn record_packet_sent(&mut self, seq_nr: u16) {
        if let Some(sent) = self
            .send_buffer
            .iter_mut()
            .find(|sent| sent.packet.seq_nr == seq_nr)
        {
            sent.sent_at = Instant::now();
        }
    }

    /// Close the connection gracefully - creates FIN packet
    pub fn close(&mut self) -> Result<UtpPacket, ConnectionError> {
        if !matches!(
            self.state,
            ConnectionState::Established | ConnectionState::Closing
        ) {
            return Err(ConnectionError::NotConnected);
        }

        let mut fin = UtpPacket::fin(
            self.remote_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.stamp_packet(&mut fin);
        self.send_buffer.push_back(SentPacket {
            packet: fin.clone(),
            sent_at: Instant::now(),
            retransmitted: false,
            selectively_acked: false,
            selectively_acked_after: 0,
            fast_retransmitted: false,
        });
        self.seq_nr = self.seq_nr.wrapping_add(1);
        self.state = ConnectionState::FinWait;
        self.last_activity = Instant::now();

        Ok(fin)
    }

    /// Send data on an established connection - creates DATA packets
    pub fn send_data(&mut self, data: &[u8]) -> Result<Vec<UtpPacket>, ConnectionError> {
        if self.state != ConnectionState::Established {
            return Err(ConnectionError::NotConnected);
        }

        let mut packets = Vec::new();
        let mut offset = 0;
        let available = self
            .congestion
            .get_window_size()
            .min(self.peer_window)
            .min(SEND_BUFFER_SIZE as u32)
            .saturating_sub(self.congestion.get_bytes_in_flight()) as usize;

        while offset < data.len() {
            let remaining = available.saturating_sub(offset);
            if remaining == 0 {
                break;
            }

            let chunk_size = std::cmp::min(remaining, data.len() - offset);
            let chunk_size = std::cmp::min(chunk_size, 1400); // MTU limit

            let mut packet = UtpPacket::data(
                self.remote_conn_id,
                self.seq_nr,
                self.ack_nr,
                self.recv_window,
                data[offset..offset + chunk_size].to_vec(),
            );
            self.stamp_packet(&mut packet);

            self.send_buffer.push_back(SentPacket {
                packet: packet.clone(),
                sent_at: Instant::now(),
                retransmitted: false,
                selectively_acked: false,
                selectively_acked_after: 0,
                fast_retransmitted: false,
            });
            self.congestion.on_data_sent(chunk_size as u32);
            self.seq_nr = self.seq_nr.wrapping_add(1);
            offset += chunk_size;

            packets.push(packet);
        }

        Ok(packets)
    }

    /// Handle incoming ACK packet
    pub(super) fn handle_ack_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<PacketHandling, ConnectionError> {
        let extensions = packet.parse_extensions()?;
        // Transition from SynSent to Established on first ACK
        let acknowledged_syn = (self.state == ConnectionState::SynSent).then_some(self.syn_seq_nr);
        if self.state == ConnectionState::SynSent {
            if packet.ack_nr != self.syn_seq_nr {
                return Err(ConnectionError::InvalidPacket(
                    "SYN was not acknowledged".to_string(),
                ));
            }
            self.ack_nr = packet.seq_nr;
            self.state = ConnectionState::Established;
        }
        let (mut acknowledged, fast_retransmit) =
            self.acknowledge_sent(packet, extensions.selective_ack)?;
        if let Some(seq_nr) = acknowledged_syn {
            acknowledged.push(seq_nr);
            if !self.syn_retransmitted
                && let Some(sent_at) = self.syn_sent_at.take()
            {
                self.add_rtt_sample(sent_at.elapsed());
            }
        }

        Ok(PacketHandling {
            response_packets: Vec::new(),
            acknowledged,
            fast_retransmit,
        })
    }

    pub(super) fn acknowledge_sent(
        &mut self,
        packet: &UtpPacket,
        selective_ack: Option<&[u8]>,
    ) -> Result<(Vec<u16>, Vec<u16>), ConnectionError> {
        self.peer_window = packet.wnd_size;
        if packet.ack_nr.wrapping_sub(self.seq_nr.wrapping_sub(1)) as i16 > 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let duplicate_cumulative_ack = self.record_peer_ack(packet.ack_nr);
        let mut acknowledged = Vec::new();
        let mut bytes_acknowledged = 0u32;
        let mut fin_acknowledged = false;
        while self
            .send_buffer
            .front()
            .is_some_and(|sent| packet.ack_nr.wrapping_sub(sent.packet.seq_nr) < 0x8000)
        {
            let sent = self.send_buffer.pop_front().unwrap();
            fin_acknowledged |= sent.packet.packet_type().ok() == Some(PacketType::StFin);
            bytes_acknowledged =
                bytes_acknowledged.saturating_add(sent.packet.payload.len() as u32);
            acknowledged.push(sent.packet.seq_nr);
            if !sent.retransmitted && !sent.selectively_acked {
                self.add_rtt_sample(sent.sent_at.elapsed());
            }
        }

        let newly_selectively_acked = self
            .send_buffer
            .iter()
            .filter(|sent| {
                !sent.selectively_acked
                    && selective_ack.is_some_and(|mask| {
                        sack_mask_acknowledges(packet.ack_nr, mask, sent.packet.seq_nr)
                    })
            })
            .map(|sent| sent.packet.seq_nr)
            .collect::<Vec<_>>();
        let mut fast_retransmit = Vec::new();
        for sent in &mut self.send_buffer {
            if sent.selectively_acked || newly_selectively_acked.contains(&sent.packet.seq_nr) {
                continue;
            }
            let later_acks = newly_selectively_acked
                .iter()
                .filter(|acked| sequence_is_after(**acked, sent.packet.seq_nr))
                .count() as u16;
            sent.selectively_acked_after = sent.selectively_acked_after.saturating_add(later_acks);
            if sent.selectively_acked_after >= 3 && !sent.fast_retransmitted && !sent.retransmitted
            {
                sent.fast_retransmitted = true;
                fast_retransmit.push(sent.packet.seq_nr);
            }
        }

        if duplicate_cumulative_ack && self.duplicate_ack_count >= 3 {
            let missing_sequence = packet.ack_nr.wrapping_add(1);
            if let Some(missing) = self.send_buffer.iter_mut().find(|sent| {
                sent.packet.seq_nr == missing_sequence
                    && !sent.selectively_acked
                    && !sent.retransmitted
            }) && !missing.fast_retransmitted
            {
                missing.fast_retransmitted = true;
                if !fast_retransmit.contains(&missing_sequence) {
                    fast_retransmit.push(missing_sequence);
                }
            }
        }

        if !newly_selectively_acked.is_empty() {
            let mut remaining = VecDeque::with_capacity(self.send_buffer.len());
            while let Some(mut sent) = self.send_buffer.pop_front() {
                if newly_selectively_acked.contains(&sent.packet.seq_nr) {
                    acknowledged.push(sent.packet.seq_nr);
                    if !sent.retransmitted {
                        self.add_rtt_sample(sent.sent_at.elapsed());
                    }
                    if sent.packet.packet_type().ok() == Some(PacketType::StFin) {
                        sent.selectively_acked = true;
                        remaining.push_back(sent);
                    } else {
                        bytes_acknowledged =
                            bytes_acknowledged.saturating_add(sent.packet.payload.len() as u32);
                    }
                } else {
                    remaining.push_back(sent);
                }
            }
            self.send_buffer = remaining;
        }

        if bytes_acknowledged > 0 {
            self.congestion.on_ack_received(
                u64::from(packet.timestamp_difference_microseconds),
                bytes_acknowledged,
            );
        }
        if fin_acknowledged {
            self.local_fin_acked = true;
            self.state = ConnectionState::Closed;
        }
        Ok((acknowledged, fast_retransmit))
    }

    fn record_peer_ack(&mut self, ack_nr: u16) -> bool {
        let Some(last_ack_nr) = self.last_peer_ack_nr else {
            self.last_peer_ack_nr = Some(ack_nr);
            self.duplicate_ack_count = 0;
            return false;
        };

        let distance = ack_nr.wrapping_sub(last_ack_nr);
        if distance == 0 {
            let missing_sequence = ack_nr.wrapping_add(1);
            let has_outstanding_gap = self
                .send_buffer
                .iter()
                .any(|sent| sent.packet.seq_nr == missing_sequence && !sent.selectively_acked);
            if !has_outstanding_gap {
                self.duplicate_ack_count = 0;
                return false;
            }
            self.duplicate_ack_count = self.duplicate_ack_count.saturating_add(1);
            true
        } else if distance < 0x8000 {
            self.last_peer_ack_nr = Some(ack_nr);
            self.duplicate_ack_count = 0;
            false
        } else {
            false
        }
    }

    fn add_rtt_sample(&mut self, sample: Duration) {
        self.rtt_estimator
            .get_or_insert_with(RttEstimator::new)
            .add_sample(sample.as_micros().min(u64::MAX as u128) as u64);
    }
}

fn sack_mask_acknowledges(ack_nr: u16, mask: &[u8], sequence: u16) -> bool {
    let distance = sequence.wrapping_sub(ack_nr);
    if !(2..0x8000).contains(&distance) {
        return false;
    }
    let bit = usize::from(distance) - 2;
    mask.get(bit / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

fn sequence_is_after(sequence: u16, reference: u16) -> bool {
    let distance = sequence.wrapping_sub(reference);
    distance > 0 && distance < 0x8000
}
