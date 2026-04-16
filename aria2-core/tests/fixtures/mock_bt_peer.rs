#![allow(dead_code)]
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct MockBtPeerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MockBtPeerServer {
    pub async fn start(info_hash: [u8; 20], piece_data: Vec<Vec<u8>>) -> Self {
        Self::start_with_metadata(info_hash, piece_data, None).await
    }

    pub async fn start_with_metadata(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Option<Vec<u8>>,
    ) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("绑定Mock Peer端口失败");
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
                                let md = torrent_metadata.clone();
                                tokio::spawn(async move { Self::handle_peer(&mut stream, &ih, &pd, md.as_deref()).await; });
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        MockBtPeerServer {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    async fn handle_peer(
        stream: &mut tokio::net::TcpStream,
        expected_info_hash: &[u8; 20],
        piece_data: &[Vec<u8>],
        torrent_metadata: Option<&[u8]>,
    ) {
        const PROTOCOL_STR: &[u8] = b"BitTorrent protocol";

        let mut handshake_buf = [0u8; 68];
        if stream.read_exact(&mut handshake_buf).await.is_err() {
            return;
        }

        let pstrlen = handshake_buf[0] as usize;
        if pstrlen != 19 {
            return;
        }
        if &handshake_buf[1..=19] != PROTOCOL_STR {
            return;
        }
        if &handshake_buf[28..48] != expected_info_hash.as_slice() {
            return;
        }

        let peer_id: [u8; 20] = rand::random();
        let mut response_hs = [0u8; 68];
        response_hs[0] = 19;
        response_hs[1..=19].copy_from_slice(PROTOCOL_STR);
        response_hs[20..28].copy_from_slice(&[0x01, 0, 0, 0, 0, 0x02, 0, 0]);
        response_hs[28..48].copy_from_slice(expected_info_hash);
        response_hs[48..68].copy_from_slice(&peer_id);

        stream.write_all(&response_hs).await.ok();
        stream.flush().await.ok();

        let num_pieces = piece_data.len() as u32;
        let bf_len = num_pieces.div_ceil(8) as usize;
        let mut bitfield = vec![0xFFu8; bf_len];
        let last_byte_bits = (num_pieces % 8) as u8;
        if last_byte_bits > 0
            && last_byte_bits < 8
            && let Some(last) = bitfield.last_mut()
        {
            *last = !((1 << last_byte_bits) - 1);
        }

        let msg_bitfield = build_message(5, &bitfield);
        stream.write_all(&msg_bitfield).await.ok();

        loop {
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let msg_len = u32::from_be_bytes(len_buf);

            if msg_len == 0 {
                continue;
            }
            if msg_len > 131072 {
                break;
            }

            let mut payload = vec![0u8; msg_len as usize];
            if stream.read_exact(&mut payload).await.is_err() {
                break;
            }

            match payload.first().copied() {
                Some(2) => {
                    let unchoke_msg = build_message(1, &[]);
                    stream.write_all(&unchoke_msg).await.ok();
                    stream.flush().await.ok();
                }
                Some(3) => {}
                Some(6) => {
                    if payload.len() >= 13 {
                        let index =
                            u32::from_be_bytes(payload[1..5].try_into().unwrap_or([0u8; 4]));
                        let begin =
                            u32::from_be_bytes(payload[5..9].try_into().unwrap_or([0u8; 4]));
                        let length =
                            u32::from_be_bytes(payload[9..13].try_into().unwrap_or([0u8; 4]));

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
                Some(20) => {
                    if let Some(meta) = torrent_metadata
                        && payload.len() > 1
                        && let Some(ext_dict) = parse_bencode_from_slice(&payload[1..])
                        && (has_key(&ext_dict, b"m") || has_key(&ext_dict, b"msg_type"))
                    {
                        let ext_resp = handle_extension_message(&ext_dict, meta);
                        if let Some(resp) = ext_resp {
                            stream.write_all(&resp).await.ok();
                            stream.flush().await.ok();
                        }
                    }
                }
                Some(7) | Some(4) | Some(5) | Some(0) | Some(1) => {}
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

fn build_extended_message(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(20);
    buf.extend_from_slice(payload);
    buf
}

fn parse_bencode_from_slice(
    data: &[u8],
) -> Option<std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>> {
    use std::collections::BTreeMap;

    fn decode_value(data: &[u8], pos: usize) -> Option<(BencodeValueForMock, usize)> {
        if pos >= data.len() {
            return None;
        }
        match data[pos] {
            b'i' => {
                let end = data[pos + 1..].iter().position(|&c| c == b'e')? + pos + 1;
                let num_str = std::str::from_utf8(&data[pos + 1..end]).ok()?;
                let val: i64 = num_str.parse().ok()?;
                Some((BencodeValueForMock::Int(val), end + 1))
            }
            b'0'..=b'9' => {
                let colon_pos = data[pos..].iter().position(|&c| c == b':')? + pos;
                let len: usize = std::str::from_utf8(&data[pos..colon_pos])
                    .ok()?
                    .parse()
                    .ok()?;
                let end = colon_pos + 1 + len;
                if end > data.len() {
                    return None;
                }
                Some((
                    BencodeValueForMock::Bytes(data[colon_pos + 1..end].to_vec()),
                    end,
                ))
            }
            b'l' => {
                if data[pos + 1] == b'e' {
                    return Some((BencodeValueForMock::List(vec![]), pos + 2));
                }
                let mut list = Vec::new();
                let mut p = pos + 1;
                while p < data.len() && data[p] != b'e' {
                    let (val, next_p) = decode_value(data, p)?;
                    list.push(val);
                    p = next_p;
                }
                Some((BencodeValueForMock::List(list), p + 1))
            }
            b'd' => {
                if data[pos + 1] == b'e' {
                    return Some((BencodeValueForMock::Dict(BTreeMap::new()), pos + 2));
                }
                let mut dict = BTreeMap::new();
                let mut p = pos + 1;
                while p < data.len() && data[p] != b'e' {
                    let (key, next_p) = decode_value(data, p)?;
                    let key_bytes = key.into_bytes().unwrap_or_default();
                    let (val, val_next_p) = decode_value(data, next_p)?;
                    dict.insert(key_bytes, val);
                    p = val_next_p;
                }
                Some((BencodeValueForMock::Dict(dict), p + 1))
            }
            _ => None,
        }
    }

    decode_value(data, 0).and_then(|(v, _)| v.into_dict())
}

enum BencodeValueForMock {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BencodeValueForMock>),
    Dict(std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>),
}
impl BencodeValueForMock {
    fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }
    fn into_dict(self) -> Option<std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>> {
        match self {
            Self::Dict(d) => Some(d),
            _ => None,
        }
    }
}

fn has_key(dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>, key: &[u8]) -> bool {
    dict.iter().any(|(k, _)| k.as_slice() == key)
}

fn find_entry<'a>(
    dict: &'a std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
    key: &[u8],
) -> Option<&'a BencodeValueForMock> {
    dict.iter()
        .find(|(k, _)| k.as_slice() == key)
        .map(|(_, v)| v)
}

fn handle_extension_message(
    dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
    metadata: &[u8],
) -> Option<Vec<u8>> {
    if find_entry(dict, b"msg_type").and_then(|v| match v {
        BencodeValueForMock::Int(i) => Some(*i),
        _ => None,
    }) == Some(0)
    {
        let piece_size = 16 * 1024;
        let total_size = metadata.len() as u64;
        let num_pieces = total_size.div_ceil(piece_size as u64) as u32;

        let mut responses = Vec::new();
        for i in 0..num_pieces {
            let offset = (i as usize) * piece_size as usize;
            let end = std::cmp::min(offset + piece_size as usize, metadata.len());
            let chunk = &metadata[offset..end];

            use std::collections::BTreeMap;
            let mut resp_dict = BTreeMap::new();
            resp_dict.insert(b"msg_type".to_vec(), BencodeValueForMock::Int(1));
            resp_dict.insert(b"piece".to_vec(), BencodeValueForMock::Int(i as i64));
            resp_dict.insert(b"data".to_vec(), BencodeValueForMock::Bytes(chunk.to_vec()));

            let mut encoded = Vec::new();
            encode_bencode_dict_for_mock(&resp_dict, &mut encoded);

            responses.push(build_extended_message(&encoded));
        }

        let mut all = Vec::new();
        for r in responses {
            all.extend(r);
        }
        return Some(all);
    }

    let mut hs_dict = std::collections::BTreeMap::new();
    let mut m_dict = std::collections::BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValueForMock::Int(1));
    hs_dict.insert(b"m".to_vec(), BencodeValueForMock::Dict(m_dict));
    hs_dict.insert(
        b"metadata_size".to_vec(),
        BencodeValueForMock::Int(metadata.len() as i64),
    );

    let mut encoded = Vec::new();
    encode_bencode_dict_for_mock(&hs_dict, &mut encoded);
    Some(build_extended_message(&encoded))
}

fn encode_bencode_dict_for_mock(
    dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
    out: &mut Vec<u8>,
) {
    out.push(b'd');
    for (k, v) in dict {
        let len_str = k.len().to_string();
        out.extend_from_slice(len_str.as_bytes());
        out.push(b':');
        out.extend_from_slice(k);
        encode_value_for_mock(v, out);
    }
    out.push(b'e')
}

fn encode_value_for_mock(val: &BencodeValueForMock, out: &mut Vec<u8>) {
    match val {
        BencodeValueForMock::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        BencodeValueForMock::Bytes(b) => {
            let len_str = b.len().to_string();
            out.extend_from_slice(len_str.as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        BencodeValueForMock::List(items) => {
            out.push(b'l');
            for item in items {
                encode_value_for_mock(item, out);
            }
            out.push(b'e');
        }
        BencodeValueForMock::Dict(d) => {
            encode_bencode_dict_for_mock(d, out);
        }
    }
}

impl Drop for MockBtPeerServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
