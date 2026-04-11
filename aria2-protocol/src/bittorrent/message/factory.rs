use super::types::{BtMessage, MessageType, PieceBlockRequest};
use tracing::debug;

pub fn parse_message(data: &[u8]) -> Result<Option<BtMessage>, String> {
    if data.is_empty() {
        return Ok(None);
    }

    if data.len() >= 4 && data == [0, 0, 0, 0] {
        return Ok(Some(BtMessage::KeepAlive));
    }

    if data.len() < 4 {
        return Err(format!("消息长度不足: {} 字节", data.len()));
    }

    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if len == 0 {
        return Ok(Some(BtMessage::KeepAlive));
    }

    if data.len() < 4 + len {
        return Err(format!(
            "消息不完整: 声明长度={}, 实际数据={}",
            len,
            data.len()
        ));
    }

    if len < 1 {
        return Err("消息体长度为0但非keepalive".to_string());
    }

    let msg_type = MessageType::try_from(data[4])?;
    let payload = &data[5..4 + len];

    debug!(
        "解析BT消息: type={:?}, payload_len={}",
        msg_type,
        payload.len()
    );

    match msg_type {
        MessageType::Choke => Ok(Some(BtMessage::Choke)),
        MessageType::Unchoke => Ok(Some(BtMessage::Unchoke)),
        MessageType::Interested => Ok(Some(BtMessage::Interested)),
        MessageType::NotInterested => Ok(Some(BtMessage::NotInterested)),
        MessageType::Have => parse_have(payload),
        MessageType::Bitfield => parse_bitfield(payload),
        MessageType::Request => parse_block_op(payload, true),
        MessageType::Piece => parse_piece(payload),
        MessageType::Cancel => parse_block_op(payload, false),
        MessageType::Port => parse_port(payload),
        MessageType::AllowedFast => parse_allowed_fast(payload),
        MessageType::Suggest => parse_suggest(payload),
        MessageType::Reject => parse_reject(payload),
        MessageType::HaveAll => Ok(Some(BtMessage::HaveAll)),
        MessageType::HaveNone => Ok(Some(BtMessage::HaveNone)),
    }
}

fn parse_have(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 4 {
        return Err(format!("Have消息payload不足: {}字节", payload.len()));
    }
    let piece_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::Have { piece_index }))
}

fn parse_bitfield(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.is_empty() {
        return Err("Bitfield消息payload为空".to_string());
    }
    Ok(Some(BtMessage::Bitfield {
        data: payload.to_vec(),
    }))
}

fn parse_block_op(payload: &[u8], is_request: bool) -> Result<Option<BtMessage>, String> {
    if payload.len() < 12 {
        return Err(format!(
            "{}消息payload不足: {}字节",
            if is_request { "Request" } else { "Cancel" },
            payload.len()
        ));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let length = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let request = PieceBlockRequest::new(index, begin, length);
    Ok(if is_request {
        Some(BtMessage::Request { request })
    } else {
        Some(BtMessage::Cancel { request })
    })
}

fn parse_piece(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 8 {
        return Err(format!("Piece消息payload不足: {}字节", payload.len()));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let data = payload[8..].to_vec();
    Ok(Some(BtMessage::Piece { index, begin, data }))
}

fn parse_port(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 2 {
        return Err(format!("Port消息payload不足: {}字节", payload.len()));
    }
    let port = u16::from_be_bytes([payload[0], payload[1]]);
    Ok(Some(BtMessage::Port { port }))
}

fn parse_allowed_fast(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 4 {
        return Err(format!("AllowedFast消息payload不足: {}字节", payload.len()));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::AllowedFast { index }))
}

fn parse_suggest(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 4 {
        return Err(format!("Suggest消息payload不足: {}字节", payload.len()));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::Suggest { index }))
}

fn parse_reject(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() < 12 {
        return Err(format!("Reject消息payload不足: {}字节", payload.len()));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let offset = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let length = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
    Ok(Some(BtMessage::Reject {
        index,
        offset,
        length,
    }))
}

pub fn parse_message_stream(buffer: &[u8]) -> Vec<(Option<BtMessage>, usize)> {
    let mut results = Vec::new();
    let mut pos = 0;
    while pos < buffer.len() {
        if buffer[pos..].len() < 4 {
            break;
        }
        let len = u32::from_be_bytes([
            buffer[pos],
            buffer[pos + 1],
            buffer[pos + 2],
            buffer[pos + 3],
        ]) as usize;
        if len == 0 {
            results.push((Some(BtMessage::KeepAlive), 4));
            pos += 4;
            continue;
        }
        let total_msg_size = 4 + len;
        if pos + total_msg_size > buffer.len() {
            break;
        }
        match parse_message(&buffer[pos..pos + total_msg_size]) {
            Ok(msg) => results.push((msg, total_msg_size)),
            Err(e) => {
                tracing::warn!("解析消息失败: {}, 跳过", e);
                break;
            }
        }
        pos += total_msg_size;
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_keepalive() {
        let msg = parse_message(&[0, 0, 0, 0]).unwrap();
        assert_eq!(msg, Some(BtMessage::KeepAlive));
    }

    #[test]
    fn test_parse_choke() {
        let msg = parse_message(&[0, 0, 0, 1, 0]).unwrap();
        assert_eq!(msg, Some(BtMessage::Choke));
    }

    #[test]
    fn test_parse_unchoke() {
        let msg = parse_message(&[0, 0, 0, 1, 1]).unwrap();
        assert_eq!(msg, Some(BtMessage::Unchoke));
    }

    #[test]
    fn test_parse_interested() {
        let msg = parse_message(&[0, 0, 0, 1, 2]).unwrap();
        assert_eq!(msg, Some(BtMessage::Interested));
    }

    #[test]
    fn test_parse_not_interested() {
        let msg = parse_message(&[0, 0, 0, 1, 3]).unwrap();
        assert_eq!(msg, Some(BtMessage::NotInterested));
    }

    #[test]
    fn test_parse_have() {
        let mut data = vec![0, 0, 0, 5, 4];
        data.extend_from_slice(&(99u32).to_be_bytes());
        let msg = parse_message(&data).unwrap();
        assert_eq!(msg, Some(BtMessage::Have { piece_index: 99 }));
    }

    #[test]
    fn test_parse_bitfield() {
        let data = vec![0, 0, 0, 3, 5, 0xFF, 0x00];
        let msg = parse_message(&data).unwrap();
        assert_eq!(
            msg,
            Some(BtMessage::Bitfield {
                data: vec![0xFF, 0x00]
            })
        );
    }

    #[test]
    fn test_parse_request() {
        let mut data = vec![0, 0, 0, 13, 6];
        data.extend_from_slice(&(1u32).to_be_bytes());
        data.extend_from_slice(&(1024u32).to_be_bytes());
        data.extend_from_slice(&(16384u32).to_be_bytes());
        let msg = parse_message(&data).unwrap();
        assert_eq!(
            msg,
            Some(BtMessage::Request {
                request: PieceBlockRequest::new(1, 1024, 16384)
            })
        );
    }

    #[test]
    fn test_parse_piece() {
        let block_data = b"hi";
        let total_len: u32 = 9 + block_data.len() as u32;
        let mut data = total_len.to_be_bytes().to_vec();
        data.push(7);
        data.extend_from_slice(&(0u32).to_be_bytes());
        data.extend_from_slice(&(0u32).to_be_bytes());
        data.extend_from_slice(block_data);
        let msg = parse_message(&data).unwrap();
        assert_eq!(
            msg,
            Some(BtMessage::Piece {
                index: 0,
                begin: 0,
                data: b"hi".to_vec()
            })
        );
    }

    #[test]
    fn test_parse_cancel() {
        let mut data = vec![0, 0, 0, 13, 8];
        data.extend_from_slice(&(5u32).to_be_bytes());
        data.extend_from_slice(&(200u32).to_be_bytes());
        data.extend_from_slice(&(8192u32).to_be_bytes());
        let msg = parse_message(&data).unwrap();
        assert_eq!(
            msg,
            Some(BtMessage::Cancel {
                request: PieceBlockRequest::new(5, 200, 8192)
            })
        );
    }

    #[test]
    fn test_parse_invalid_id() {
        let err = parse_message(&[0, 0, 0, 1, 255]);
        assert!(err.is_err());
    }

    #[test]
    fn test_parse_stream_multiple() {
        let mut stream = vec![];
        stream.extend_from_slice(&[0, 0, 0, 0]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0]);
        stream.extend_from_slice(&[0, 0, 0, 1, 1]);

        let msgs = parse_message_stream(&stream);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].0, Some(BtMessage::KeepAlive));
        assert_eq!(msgs[1].0, Some(BtMessage::Choke));
        assert_eq!(msgs[2].0, Some(BtMessage::Unchoke));
    }

    #[test]
    fn test_empty_input() {
        assert!(parse_message(&[]).unwrap().is_none());
    }

    #[test]
    fn test_truncated_message() {
        let err = parse_message(&[0, 0, 0, 5, 4, 0, 0]);
        assert!(err.is_err());
    }

    // --- Phase 13 / Wave A — Task A6: BT Message Unit Tests ---

    #[test]
    fn test_a6_01_allowed_fast_roundtrip_index_42() {
        use super::super::serializer::serialize_allowed_fast;
        let msg = BtMessage::AllowedFast { index: 42 };
        let bytes = serialize_allowed_fast(42);
        let decoded = parse_message(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        if let BtMessage::AllowedFast { index } = decoded {
            assert_eq!(index, 42);
        }
    }

    #[test]
    fn test_a6_02_reject_roundtrip() {
        use super::super::serializer::serialize_reject;
        let msg = BtMessage::Reject {
            index: 10,
            offset: 100,
            length: 16384,
        };
        let bytes = serialize_reject(10, 100, 16384);
        let decoded = parse_message(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        if let BtMessage::Reject {
            index,
            offset,
            length,
        } = decoded
        {
            assert_eq!(index, 10);
            assert_eq!(offset, 100);
            assert_eq!(length, 16384);
        }
    }

    #[test]
    fn test_a6_03_suggest_roundtrip_index_7() {
        use super::super::serializer::serialize_suggest;
        let msg = BtMessage::Suggest { index: 7 };
        let bytes = serialize_suggest(7);
        let decoded = parse_message(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        if let BtMessage::Suggest { index } = decoded {
            assert_eq!(index, 7);
        }
    }

    #[test]
    fn test_a6_04_have_all_roundtrip() {
        use super::super::serializer::serialize_have_all;
        let msg = BtMessage::HaveAll;
        let bytes = serialize_have_all();
        let decoded = parse_message(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(matches!(decoded, BtMessage::HaveAll));
    }

    #[test]
    fn test_a6_05_have_none_roundtrip() {
        use super::super::serializer::serialize_have_none;
        let msg = BtMessage::HaveNone;
        let bytes = serialize_have_none();
        let decoded = parse_message(&bytes).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert!(matches!(decoded, BtMessage::HaveNone));
    }

    #[test]
    fn test_a6_06_parse_allowed_fast_dispatch_correct_fields() {
        let mut data = vec![0, 0, 0, 5, 11];
        data.extend_from_slice(&(99u32).to_be_bytes());
        let msg = parse_message(&data).unwrap().unwrap();
        assert!(matches!(msg, BtMessage::AllowedFast { .. }));
        if let BtMessage::AllowedFast { index } = msg {
            assert_eq!(index, 99);
        }
    }

    #[test]
    fn test_a6_07_parse_reject_dispatch_correct_fields() {
        let mut data = vec![0, 0, 0, 13, 13];
        data.extend_from_slice(&(5u32).to_be_bytes());
        data.extend_from_slice(&(200u32).to_be_bytes());
        data.extend_from_slice(&(8192u32).to_be_bytes());
        let msg = parse_message(&data).unwrap().unwrap();
        assert!(matches!(msg, BtMessage::Reject { .. }));
        if let BtMessage::Reject {
            index,
            offset,
            length,
        } = msg
        {
            assert_eq!(index, 5);
            assert_eq!(offset, 200);
            assert_eq!(length, 8192);
        }
    }

    #[test]
    fn test_a6_08_allowed_fast_set_tracking_add_and_check() {
        use std::collections::HashSet;
        let mut allowed_fast: HashSet<u32> = HashSet::new();
        assert!(!allowed_fast.contains(&42));
        allowed_fast.insert(42);
        assert!(allowed_fast.contains(&42));
        allowed_fast.insert(7);
        allowed_fast.insert(100);
        assert_eq!(allowed_fast.len(), 3);
        assert!(allowed_fast.contains(&7));
        assert!(allowed_fast.contains(&100));
        assert!(!allowed_fast.contains(&999));
    }
}
