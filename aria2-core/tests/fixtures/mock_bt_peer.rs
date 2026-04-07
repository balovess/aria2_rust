use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct MockBtPeerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockBtPeerServer {
    pub async fn start(info_hash: [u8; 20], piece_data: Vec<Vec<u8>>) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.expect("绑定Mock Peer端口失败");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, _)) => {
                                let ih = info_hash;
                                let pd = piece_data.clone();
                                tokio::spawn(async move { Self::handle_peer(&mut stream, &ih, &pd).await; });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        MockBtPeerServer { addr: actual_addr, shutdown: Some(shutdown_tx) }
    }

    pub fn addr(&self) -> SocketAddr { self.addr }

    async fn handle_peer(stream: &mut tokio::net::TcpStream, expected_info_hash: &[u8; 20], piece_data: &[Vec<u8>]) {
        const PROTOCOL_STR: &[u8] = b"BitTorrent protocol";

        let mut handshake_buf = [0u8; 68];
        if stream.read_exact(&mut handshake_buf).await.is_err() { return; }

        let pstrlen = handshake_buf[0] as usize;
        if pstrlen != 19 { return; }
        if &handshake_buf[1..=19] != PROTOCOL_STR { return; }
        if &handshake_buf[28..48] != expected_info_hash.as_slice() { return; }

        let peer_id: [u8; 20] = rand::random();
        let mut response_hs = [0u8; 68];
        response_hs[0] = 19;
        response_hs[1..=19].copy_from_slice(PROTOCOL_STR);
        response_hs[20..28].copy_from_slice(&[0u8; 8]);
        response_hs[28..48].copy_from_slice(expected_info_hash);
        response_hs[48..68].copy_from_slice(&peer_id);

        stream.write_all(&response_hs).await.ok();
        stream.flush().await.ok();

        let num_pieces = piece_data.len() as u32;
        let bf_len = ((num_pieces + 7) / 8) as usize;
        let mut bitfield = vec![0xFFu8; bf_len];
        let last_byte_bits = (num_pieces % 8) as u8;
        if last_byte_bits > 0 && last_byte_bits < 8 {
            if let Some(last) = bitfield.last_mut() {
                *last &= (0xFF << (8 - last_byte_bits)) | 0xFF >> last_byte_bits;
            }
        }

        let msg_bitfield = build_message(5, &bitfield);
        stream.write_all(&msg_bitfield).await.ok();

        loop {
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).await.is_err() { break; }
            let msg_len = u32::from_be_bytes(len_buf);

            if msg_len == 0 { continue; }
            if msg_len > 131072 { break; }

            let mut payload = vec![0u8; msg_len as usize];
            if stream.read_exact(&mut payload).await.is_err() { break; }

            match payload.first().copied() {
                Some(2) => {
                    let unchoke_msg = build_message(1, &[]);
                    stream.write_all(&unchoke_msg).await.ok();
                    stream.flush().await.ok();
                }
                Some(3) => {}
                Some(6) => {
                    if payload.len() >= 13 {
                        let index = u32::from_be_bytes(payload[1..5].try_into().unwrap_or([0u8; 4]));
                        let begin = u32::from_be_bytes(payload[5..9].try_into().unwrap_or([0u8; 4]));
                        let length = u32::from_be_bytes(payload[9..13].try_into().unwrap_or([0u8; 4]));

                        let data = if (index as usize) < piece_data.len() {
                            let piece = &piece_data[index as usize];
                            let begin_usize = begin as usize;
                            let length_usize = length as usize;
                            if begin_usize + length_usize <= piece.len() {
                                piece[begin_usize..begin_usize + length_usize].to_vec()
                            } else {
                                vec![0u8; length_usize]
                            }
                        } else {
                            vec![0u8; length as usize]
                        };

                        let mut piece_payload: Vec<u8> = Vec::with_capacity(12 + data.len());
                        piece_payload.extend_from_slice(&index.to_be_bytes());
                        piece_payload.extend_from_slice(&begin.to_be_bytes());
                        piece_payload.extend_from_slice(&data);
                        let piece_msg = build_message(7, &piece_payload);
                        stream.write_all(&piece_msg).await.ok();
                        stream.flush().await.ok();
                    }
                }
                Some(7) => {}
                Some(4) | Some(5) => {}
                Some(0) | Some(1) => {}
                _ => {}
            }
        }
    }
}

fn build_message(msg_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 1) as u32;
    let mut buf = Vec::with_capacity(4 + 1 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(msg_id);
    buf.extend_from_slice(payload);
    buf
}

impl Drop for MockBtPeerServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
