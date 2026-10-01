#![allow(dead_code)]
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct MockBtPeerServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    requested_pieces: std::sync::Arc<tokio::sync::Mutex<Vec<u32>>>,
    accepted_peers: std::sync::Arc<tokio::sync::Mutex<Vec<SocketAddr>>>,
    completed_handshakes: tokio::sync::watch::Sender<usize>,
    metadata_size_seen: tokio::sync::watch::Sender<u32>,
    metadata_upload: tokio::sync::watch::Sender<Option<(u32, u32, Vec<u8>)>>,
}

#[derive(Clone)]
struct MockBtPeerSignals {
    completed_handshakes: tokio::sync::watch::Sender<usize>,
    metadata_size_seen: tokio::sync::watch::Sender<u32>,
    metadata_upload: tokio::sync::watch::Sender<Option<(u32, u32, Vec<u8>)>>,
}

#[derive(Clone, Default)]
struct MockBtPeerBehavior {
    piece_response_delay: Option<std::time::Duration>,
    strict_availability: bool,
    pex_peers: Vec<SocketAddr>,
    stay_choked: bool,
    request_metadata_upload: bool,
}

impl std::fmt::Debug for MockBtPeerServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBtPeerServer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl MockBtPeerServer {
    pub async fn start(info_hash: [u8; 20], piece_data: Vec<Vec<u8>>) -> Self {
        Self::start_with_metadata(info_hash, piece_data, None).await
    }

    pub async fn start_strict_availability(info_hash: [u8; 20], piece_data: Vec<Vec<u8>>) -> Self {
        Self::start_with_policy(info_hash, piece_data, None, None, true).await
    }

    pub async fn start_staying_choked(info_hash: [u8; 20], piece_data: Vec<Vec<u8>>) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            None,
            MockBtPeerBehavior {
                stay_choked: true,
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    pub async fn start_with_response_delay(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        delay: std::time::Duration,
    ) -> Self {
        Self::start_with_metadata_and_delay(info_hash, piece_data, None, Some(delay)).await
    }

    pub async fn start_advertising_pex_peer(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        discovered_peer: SocketAddr,
        piece_response_delay: std::time::Duration,
    ) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            None,
            MockBtPeerBehavior {
                piece_response_delay: Some(piece_response_delay),
                pex_peers: vec![discovered_peer],
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    pub async fn start_advertising_pex_peers(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        discovered_peers: Vec<SocketAddr>,
        piece_response_delay: std::time::Duration,
    ) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            None,
            MockBtPeerBehavior {
                piece_response_delay: Some(piece_response_delay),
                pex_peers: discovered_peers,
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    pub async fn start_staying_choked_advertising_pex_peers(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        discovered_peers: Vec<SocketAddr>,
    ) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            None,
            MockBtPeerBehavior {
                pex_peers: discovered_peers,
                stay_choked: true,
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    pub async fn start_requesting_metadata_upload(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Vec<u8>,
    ) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            Some(torrent_metadata),
            MockBtPeerBehavior {
                piece_response_delay: Some(std::time::Duration::from_millis(200)),
                request_metadata_upload: true,
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    pub async fn start_failing() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind failing mock peer port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let (completed_handshakes, _) = tokio::sync::watch::channel(0);
        let (metadata_size_seen, _) = tokio::sync::watch::channel(0);
        let (metadata_upload, _) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if result.is_err() { break; }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });
        Self {
            addr: actual_addr,
            shutdown: Some(shutdown_tx),
            requested_pieces: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            accepted_peers: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            completed_handshakes,
            metadata_size_seen,
            metadata_upload,
        }
    }

    pub async fn start_with_metadata(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Option<Vec<u8>>,
    ) -> Self {
        Self::start_with_metadata_and_delay(info_hash, piece_data, torrent_metadata, None).await
    }

    pub async fn start_with_metadata_and_delay(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Option<Vec<u8>>,
        piece_response_delay: Option<std::time::Duration>,
    ) -> Self {
        Self::start_with_policy(
            info_hash,
            piece_data,
            torrent_metadata,
            piece_response_delay,
            false,
        )
        .await
    }

    async fn start_with_policy(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Option<Vec<u8>>,
        piece_response_delay: Option<std::time::Duration>,
        strict_availability: bool,
    ) -> Self {
        Self::start_with_policy_and_pex(
            info_hash,
            piece_data,
            torrent_metadata,
            MockBtPeerBehavior {
                piece_response_delay,
                strict_availability,
                ..MockBtPeerBehavior::default()
            },
        )
        .await
    }

    async fn start_with_policy_and_pex(
        info_hash: [u8; 20],
        piece_data: Vec<Vec<u8>>,
        torrent_metadata: Option<Vec<u8>>,
        behavior: MockBtPeerBehavior,
    ) -> Self {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind mock peer port");
        let actual_addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let requested_pieces = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let requested_pieces_for_task = std::sync::Arc::clone(&requested_pieces);
        let accepted_peers = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let accepted_peers_for_task = std::sync::Arc::clone(&accepted_peers);
        let (completed_handshakes, _) = tokio::sync::watch::channel(0usize);
        let completed_handshakes_for_task = completed_handshakes.clone();
        let (metadata_size_seen, _) = tokio::sync::watch::channel(0u32);
        let metadata_size_seen_for_task = metadata_size_seen.clone();
        let (metadata_upload, _) = tokio::sync::watch::channel(None);
        let metadata_upload_for_task = metadata_upload.clone();
        let signals = MockBtPeerSignals {
            completed_handshakes: completed_handshakes_for_task,
            metadata_size_seen: metadata_size_seen_for_task,
            metadata_upload: metadata_upload_for_task,
        };

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((mut stream, peer_addr)) => {
                                accepted_peers_for_task.lock().await.push(peer_addr);
                                let ih = info_hash;
                                let pd = piece_data.clone();
                                let md = torrent_metadata.clone();
                                let requests = std::sync::Arc::clone(&requested_pieces_for_task);
                                let behavior = behavior.clone();
                                let signals = signals.clone();
                                tokio::spawn(async move {
                                    Self::handle_peer(
                                        &mut stream,
                                        &ih,
                                        &pd,
                                        md.as_deref(),
                                        requests,
                                        behavior,
                                        signals,
                                    )
                                    .await;
                                });
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
            requested_pieces,
            accepted_peers,
            completed_handshakes,
            metadata_size_seen,
            metadata_upload,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn requested_pieces(&self) -> Vec<u32> {
        self.requested_pieces.lock().await.clone()
    }

    pub async fn accepted_peers(&self) -> Vec<SocketAddr> {
        self.accepted_peers.lock().await.clone()
    }

    pub fn completed_handshake_count(&self) -> usize {
        *self.completed_handshakes.borrow()
    }

    pub async fn wait_for_metadata_size(&self, timeout: std::time::Duration) -> Option<u32> {
        let mut metadata_size_seen = self.metadata_size_seen.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let size = *metadata_size_seen.borrow_and_update();
                if size > 0 {
                    return Some(size);
                }
                if metadata_size_seen.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn wait_for_metadata_upload(
        &self,
        timeout: std::time::Duration,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let mut metadata_upload = self.metadata_upload.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(upload) = metadata_upload.borrow_and_update().clone() {
                    return Some(upload);
                }
                if metadata_upload.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn wait_for_handshakes(&self, count: usize, timeout: std::time::Duration) -> bool {
        let mut completed = self.completed_handshakes.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                if *completed.borrow_and_update() >= count {
                    return;
                }
                if completed.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .is_ok()
    }

    async fn handle_peer(
        stream: &mut tokio::net::TcpStream,
        expected_info_hash: &[u8; 20],
        piece_data: &[Vec<u8>],
        torrent_metadata: Option<&[u8]>,
        requested_pieces: std::sync::Arc<tokio::sync::Mutex<Vec<u32>>>,
        behavior: MockBtPeerBehavior,
        signals: MockBtPeerSignals,
    ) {
        let MockBtPeerBehavior {
            piece_response_delay,
            strict_availability,
            pex_peers,
            stay_choked,
            request_metadata_upload,
        } = behavior;
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
        response_hs[20..28].copy_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        response_hs[28..48].copy_from_slice(expected_info_hash);
        response_hs[48..68].copy_from_slice(&peer_id);

        if stream.write_all(&response_hs).await.is_err() {
            return;
        }
        stream.flush().await.ok();
        signals
            .completed_handshakes
            .send_modify(|count| *count += 1);

        let num_pieces = piece_data.len() as u32;
        let bf_len = num_pieces.div_ceil(8) as usize;
        let mut bitfield = vec![0xFFu8; bf_len];
        let last_byte_bits = (num_pieces % 8) as u8;
        if last_byte_bits > 0
            && last_byte_bits < 8
            && let Some(last) = bitfield.last_mut()
        {
            *last = 0xff << (8 - last_byte_bits);
        }

        if !bitfield.is_empty() {
            let msg_bitfield = build_message(5, &bitfield);
            stream.write_all(&msg_bitfield).await.ok();
        }
        let mut client_ut_metadata_id = 1u8;
        let mut availability_sent = false;
        let mut control_sent = false;
        let mut pex_sent = false;
        let mut metadata_upload_requested = false;

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

            let message_id = payload.first().copied();
            if strict_availability {
                if matches!(message_id, Some(5 | 14 | 15)) {
                    if availability_sent || control_sent {
                        break;
                    }
                    availability_sent = true;
                } else if !matches!(message_id, Some(20)) {
                    control_sent = true;
                }
            }
            match message_id {
                Some(2) => {
                    if !stay_choked {
                        let unchoke_msg = build_message(1, &[]);
                        stream.write_all(&unchoke_msg).await.ok();
                        stream.flush().await.ok();
                    }
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
                        requested_pieces.lock().await.push(index);

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
                        if let Some(delay) = piece_response_delay {
                            tokio::time::sleep(delay).await;
                        }
                        stream.write_all(&piece_msg).await.ok();
                        stream.flush().await.ok();
                    }
                }
                Some(20) => {
                    if !pex_sent && payload.get(1) == Some(&0) && !pex_peers.is_empty() {
                        use aria2_protocol::bittorrent::message::extension::{
                            CompactPeerV4, ExtensionHandshake, UtPexMessage,
                        };

                        let mut extension_handshake = ExtensionHandshake::new();
                        extension_handshake.with_ut_pex(17);
                        let handshake = build_extended_message(0, &extension_handshake.to_bytes());

                        let mut pex = UtPexMessage::new();
                        for discovered_peer in &pex_peers {
                            if let std::net::IpAddr::V4(ip) = discovered_peer.ip() {
                                let mut compact = [0u8; 6];
                                compact[..4].copy_from_slice(&ip.octets());
                                compact[4..].copy_from_slice(&discovered_peer.port().to_be_bytes());
                                pex.added.push(CompactPeerV4(compact));
                            }
                        }
                        if pex.added.is_empty() {
                            continue;
                        }
                        let pex_message = { build_extended_message(17, &pex.to_payload()) };

                        if stream.write_all(&handshake).await.is_err()
                            || stream.write_all(&pex_message).await.is_err()
                        {
                            break;
                        }
                        pex_sent = true;
                    }
                    if let Some(meta) = torrent_metadata
                        && payload.len() > 2
                        && let Some(ext_dict) = parse_bencode_from_slice(&payload[2..])
                        && (has_key(&ext_dict, b"m") || has_key(&ext_dict, b"msg_type"))
                    {
                        if payload[1] == 0
                            && let Some(metadata_size) = find_metadata_size(&ext_dict)
                        {
                            signals.metadata_size_seen.send_replace(metadata_size);
                        }
                        if payload[1] == 0
                            && let Some(id) = find_ut_metadata_id(&ext_dict)
                        {
                            client_ut_metadata_id = id;
                        }
                        let ext_resp =
                            handle_extension_message(&ext_dict, meta, client_ut_metadata_id);
                        if let Some(resp) = ext_resp {
                            stream.write_all(&resp).await.ok();
                            stream.flush().await.ok();
                        }
                        if request_metadata_upload
                            && !metadata_upload_requested
                            && payload[1] == 0
                            && find_metadata_size(&ext_dict).is_some()
                        {
                            use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;
                            let request = build_extended_message(
                                client_ut_metadata_id,
                                &UtMetadataMessage::Request { piece: 0 }.to_payload(),
                            );
                            if stream.write_all(&request).await.is_err() {
                                break;
                            }
                            stream.flush().await.ok();
                            metadata_upload_requested = true;
                        }
                    }
                    if payload.get(1).is_some_and(|ext_id| *ext_id != 0)
                        && let Ok(aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Data {
                            piece,
                            total_size,
                            data,
                        }) = aria2_protocol::bittorrent::message::extension::UtMetadataMessage::from_payload(&payload[2..])
                    {
                        signals
                            .metadata_upload
                            .send_replace(Some((piece, total_size, data)));
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

fn build_extended_message(ext_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = (2 + payload.len()) as u32;
    let mut buf = Vec::with_capacity(6 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(20);
    buf.push(ext_id);
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

fn find_ut_metadata_id(
    dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
) -> Option<u8> {
    let extensions = match find_entry(dict, b"m")? {
        BencodeValueForMock::Dict(extensions) => extensions,
        _ => return None,
    };
    match find_entry(extensions, b"ut_metadata")? {
        BencodeValueForMock::Int(id) => u8::try_from(*id).ok().filter(|id| *id != 0),
        _ => None,
    }
}

fn find_metadata_size(
    dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
) -> Option<u32> {
    match find_entry(dict, b"metadata_size")? {
        BencodeValueForMock::Int(size) => u32::try_from(*size).ok().filter(|size| *size > 0),
        _ => None,
    }
}

fn handle_extension_message(
    dict: &std::collections::BTreeMap<Vec<u8>, BencodeValueForMock>,
    metadata: &[u8],
    response_ext_id: u8,
) -> Option<Vec<u8>> {
    if find_entry(dict, b"msg_type").and_then(|v| match v {
        BencodeValueForMock::Int(i) => Some(*i),
        _ => None,
    }) == Some(0)
    {
        let piece_size = 16 * 1024;
        let total_size = metadata.len() as u64;
        let num_pieces = total_size.div_ceil(piece_size as u64) as u32;

        let piece = find_entry(dict, b"piece").and_then(|value| match value {
            BencodeValueForMock::Int(piece) => u32::try_from(*piece).ok(),
            _ => None,
        })?;
        if piece >= num_pieces {
            return None;
        }

        let offset = (piece as usize) * piece_size as usize;
        let end = std::cmp::min(offset + piece_size as usize, metadata.len());
        let chunk = &metadata[offset..end];

        use std::collections::BTreeMap;
        let mut resp_dict = BTreeMap::new();
        resp_dict.insert(b"msg_type".to_vec(), BencodeValueForMock::Int(1));
        resp_dict.insert(b"piece".to_vec(), BencodeValueForMock::Int(piece as i64));
        resp_dict.insert(
            b"total_size".to_vec(),
            BencodeValueForMock::Int(metadata.len() as i64),
        );

        let mut encoded = Vec::new();
        encode_bencode_dict_for_mock(&resp_dict, &mut encoded);
        encoded.extend_from_slice(chunk);
        return Some(build_extended_message(response_ext_id, &encoded));
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
    Some(build_extended_message(0, &encoded))
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
