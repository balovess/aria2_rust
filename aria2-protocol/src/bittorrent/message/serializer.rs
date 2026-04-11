use super::types::{BtMessage, PieceBlockRequest};

const DEFAULT_REQUEST_LENGTH: u32 = 16384;

pub fn serialize(message: &BtMessage) -> Vec<u8> {
    match message {
        BtMessage::KeepAlive => vec![0, 0, 0, 0],
        msg => {
            let id = msg.message_id().unwrap();
            let mut result = vec![0u8; 4];
            let payload = build_payload(msg);
            let total_len = 1 + payload.len();
            result[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
            result.push(id);
            result.extend(payload);
            result
        }
    }
}

fn build_payload(message: &BtMessage) -> Vec<u8> {
    match message {
        BtMessage::Choke
        | BtMessage::Unchoke
        | BtMessage::Interested
        | BtMessage::NotInterested => vec![],
        BtMessage::Have { piece_index } => piece_index.to_be_bytes().to_vec(),
        BtMessage::Bitfield { data } => data.clone(),
        BtMessage::Request { request } => serialize_block_request(request),
        BtMessage::Cancel { request } => serialize_block_request(request),
        BtMessage::Piece { index, begin, data } => {
            let mut buf = Vec::with_capacity(9 + data.len());
            buf.extend_from_slice(&index.to_be_bytes());
            buf.extend_from_slice(&begin.to_be_bytes());
            buf.extend_from_slice(data);
            buf
        }
        BtMessage::Port { port } => port.to_be_bytes().to_vec(),
        BtMessage::AllowedFast { index } => index.to_be_bytes().to_vec(),
        BtMessage::Reject {
            index,
            offset,
            length,
        } => {
            let mut buf = vec![0u8; 12];
            buf[0..4].copy_from_slice(&index.to_be_bytes());
            buf[4..8].copy_from_slice(&offset.to_be_bytes());
            buf[8..12].copy_from_slice(&length.to_be_bytes());
            buf
        }
        BtMessage::Suggest { index } => index.to_be_bytes().to_vec(),
        BtMessage::HaveAll | BtMessage::HaveNone => vec![],
        BtMessage::KeepAlive => vec![],
    }
}

fn serialize_block_request(req: &PieceBlockRequest) -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    buf[0..4].copy_from_slice(&req.index.to_be_bytes());
    buf[4..8].copy_from_slice(&req.begin.to_be_bytes());
    buf[8..12].copy_from_slice(&req.length.to_be_bytes());
    buf
}

pub fn serialize_choke() -> Vec<u8> {
    serialize(&BtMessage::Choke)
}
pub fn serialize_unchoke() -> Vec<u8> {
    serialize(&BtMessage::Unchoke)
}
pub fn serialize_interested() -> Vec<u8> {
    serialize(&BtMessage::Interested)
}
pub fn serialize_not_interested() -> Vec<u8> {
    serialize(&BtMessage::NotInterested)
}
pub fn serialize_have(piece_index: u32) -> Vec<u8> {
    serialize(&BtMessage::Have { piece_index })
}
pub fn serialize_bitfield(data: Vec<u8>) -> Vec<u8> {
    serialize(&BtMessage::Bitfield { data })
}
pub fn serialize_request(index: u32, begin: u32, length: u32) -> Vec<u8> {
    serialize(&BtMessage::Request {
        request: PieceBlockRequest::new(index, begin, length),
    })
}
pub fn serialize_cancel(index: u32, begin: u32, length: u32) -> Vec<u8> {
    serialize(&BtMessage::Cancel {
        request: PieceBlockRequest::new(index, begin, length),
    })
}
pub fn serialize_piece(index: u32, begin: u32, data: Vec<u8>) -> Vec<u8> {
    serialize(&BtMessage::Piece { index, begin, data })
}
pub fn serialize_port(port: u16) -> Vec<u8> {
    serialize(&BtMessage::Port { port })
}
pub fn serialize_keepalive() -> Vec<u8> {
    serialize(&BtMessage::KeepAlive)
}
pub fn serialize_allowed_fast(index: u32) -> Vec<u8> {
    serialize(&BtMessage::AllowedFast { index })
}
pub fn serialize_reject(index: u32, offset: u32, length: u32) -> Vec<u8> {
    serialize(&BtMessage::Reject {
        index,
        offset,
        length,
    })
}
pub fn serialize_suggest(index: u32) -> Vec<u8> {
    serialize(&BtMessage::Suggest { index })
}
pub fn serialize_have_all() -> Vec<u8> {
    serialize(&BtMessage::HaveAll)
}
pub fn serialize_have_none() -> Vec<u8> {
    serialize(&BtMessage::HaveNone)
}

pub fn create_standard_requests(piece_index: u32, piece_size: u32, offset: u32) -> Vec<BtMessage> {
    let remaining = piece_size - offset;
    let mut requests = Vec::new();
    let mut pos = offset;
    while pos < piece_size {
        let block_len = if pos + DEFAULT_REQUEST_LENGTH <= piece_size {
            DEFAULT_REQUEST_LENGTH
        } else {
            remaining - (pos - offset)
        };
        requests.push(BtMessage::Request {
            request: PieceBlockRequest::new(piece_index, pos, block_len),
        });
        pos += block_len;
    }
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keepalive_serialization() {
        let bytes = serialize_keepalive();
        assert_eq!(bytes, [0, 0, 0, 0]);
    }

    #[test]
    fn test_choke_serialization() {
        let bytes = serialize_choke();
        assert_eq!(bytes.len(), 5);
        assert_eq!(&bytes[..4], [0, 0, 0, 1]);
        assert_eq!(bytes[4], 0);
    }

    #[test]
    fn test_have_serialization() {
        let bytes = serialize_have(42);
        assert_eq!(bytes.len(), 9);
        assert_eq!(bytes[4], 4);
        assert_eq!(&bytes[5..9], (42u32).to_be_bytes().as_ref());
    }

    #[test]
    fn test_request_serialization() {
        let bytes = serialize_request(10, 20, 30);
        assert_eq!(bytes.len(), 17);
        assert_eq!(bytes[4], 6);
        assert_eq!(&bytes[5..9], (10u32).to_be_bytes().as_ref());
        assert_eq!(&bytes[9..13], (20u32).to_be_bytes().as_ref());
        assert_eq!(&bytes[13..17], (30u32).to_be_bytes().as_ref());
    }

    #[test]
    fn test_piece_serialization() {
        let data = b"block_data";
        let bytes = serialize_piece(5, 100, data.to_vec());
        assert_eq!(bytes.len(), 13 + data.len());
        assert_eq!(bytes[4], 7);
        assert_eq!(&bytes[5..9], (5u32).to_be_bytes().as_ref());
        assert_eq!(&bytes[9..13], (100u32).to_be_bytes().as_ref());
        assert_eq!(&bytes[13..], b"block_data");
    }

    #[test]
    fn test_bitfield_serialization() {
        let bf = vec![0xFF, 0x00, 0xF0];
        let bytes = serialize_bitfield(bf.clone());
        assert_eq!(bytes.len(), 8);
        assert_eq!(&bytes[5..], &bf);
    }

    #[test]
    fn test_create_standard_requests() {
        let reqs = create_standard_requests(0, 50000, 0);
        let expected_count = (50000 + DEFAULT_REQUEST_LENGTH - 1) / DEFAULT_REQUEST_LENGTH;
        assert_eq!(reqs.len(), expected_count as usize);

        let last_req = reqs.last().unwrap();
        if let BtMessage::Request { request } = last_req {
            assert_eq!(request.index, 0);
            let total_requested: u32 = reqs
                .iter()
                .map(|r| {
                    if let BtMessage::Request { request } = r {
                        request.length
                    } else {
                        0
                    }
                })
                .sum();
            assert_eq!(total_requested, 50000);
        }
    }
}
