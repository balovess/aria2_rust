use super::MAX_BT_MESSAGE_LENGTH;
use super::types::{BtMessage, MessageType, PieceBlockRequest};
use bytes::Bytes;
use tracing::debug;

pub fn parse_message(data: &[u8]) -> Result<Option<BtMessage>, String> {
    if data.is_empty() {
        return Ok(None);
    }

    if data.len() >= 4 && data == [0, 0, 0, 0] {
        return Ok(Some(BtMessage::KeepAlive));
    }

    if data.len() < 4 {
        return Err(format!("Insufficient message length: {} bytes", data.len()));
    }

    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if len > MAX_BT_MESSAGE_LENGTH {
        return Err(format!(
            "BitTorrent message length {} exceeds maximum {}",
            len, MAX_BT_MESSAGE_LENGTH
        ));
    }
    if len == 0 {
        return Ok(Some(BtMessage::KeepAlive));
    }

    if data.len() < 4 + len {
        return Err(format!(
            "Incomplete message: declared length={}, actual data={}",
            len,
            data.len()
        ));
    }

    if len < 1 {
        return Err("Message body length is 0 but not keepalive".to_string());
    }

    let msg_type = MessageType::try_from(data[4])?;
    let payload = &data[5..4 + len];

    debug!(
        "Parsing BT message: type={:?}, payload_len={}",
        msg_type,
        payload.len()
    );

    match msg_type {
        MessageType::Choke => parse_zero_payload(payload, "Choke"),
        MessageType::Unchoke => parse_zero_payload(payload, "Unchoke"),
        MessageType::Interested => parse_zero_payload(payload, "Interested"),
        MessageType::NotInterested => parse_zero_payload(payload, "NotInterested"),
        MessageType::Have => parse_have(payload),
        MessageType::Bitfield => parse_bitfield(payload),
        MessageType::Request => parse_block_op(payload, true),
        MessageType::Piece => parse_piece(payload),
        MessageType::Cancel => parse_block_op(payload, false),
        MessageType::Port => parse_port(payload),
        MessageType::AllowedFast => parse_allowed_fast(payload),
        MessageType::Suggest => parse_suggest(payload),
        MessageType::Reject => parse_reject(payload),
        MessageType::HaveAll => parse_zero_payload(payload, "HaveAll"),
        MessageType::HaveNone => parse_zero_payload(payload, "HaveNone"),
        MessageType::Extended => parse_extended(payload),
    }
}

fn parse_zero_payload(payload: &[u8], message_name: &str) -> Result<Option<BtMessage>, String> {
    if !payload.is_empty() {
        return Err(format!(
            "{} message payload must be empty, got {} bytes",
            message_name,
            payload.len()
        ));
    }

    let message = match message_name {
        "Choke" => BtMessage::Choke,
        "Unchoke" => BtMessage::Unchoke,
        "Interested" => BtMessage::Interested,
        "NotInterested" => BtMessage::NotInterested,
        "HaveAll" => BtMessage::HaveAll,
        "HaveNone" => BtMessage::HaveNone,
        _ => return Err(format!("unsupported zero-payload message: {message_name}")),
    };
    Ok(Some(message))
}

fn parse_have(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() != 4 {
        return Err(format!(
            "Have message payload must be 4 bytes, got {}",
            payload.len()
        ));
    }
    let piece_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::Have { piece_index }))
}

fn parse_bitfield(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.is_empty() {
        return Err("Bitfield message payload is empty".to_string());
    }
    Ok(Some(BtMessage::Bitfield {
        data: payload.to_vec(),
    }))
}

fn parse_block_op(payload: &[u8], is_request: bool) -> Result<Option<BtMessage>, String> {
    if payload.len() != 12 {
        return Err(format!(
            "{} message payload must be 12 bytes, got {}",
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
    if payload.len() <= 8 {
        return Err(format!(
            "Piece message payload must contain data after the 8-byte header, got {} bytes",
            payload.len()
        ));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let data = Bytes::copy_from_slice(&payload[8..]);
    Ok(Some(BtMessage::Piece { index, begin, data }))
}

/// Parse a complete frame while retaining the frame allocation for Piece data.
///
/// `BytesMut::freeze` can hand the frame to this function without copying. A
/// Piece payload is then a shared slice of that frame; other message kinds use
/// the established parser because their payloads are small or need owned
/// protocol-specific representations.
pub fn parse_message_bytes(data: Bytes) -> Result<Option<BtMessage>, String> {
    if data.len() < 5 {
        return parse_message(&data);
    }

    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if len > MAX_BT_MESSAGE_LENGTH {
        return Err(format!(
            "BitTorrent message length {} exceeds maximum {}",
            len, MAX_BT_MESSAGE_LENGTH
        ));
    }
    if len == 0 || data.len() < 4 + len {
        return parse_message(&data);
    }

    if data[4] == MessageType::Piece as u8 {
        if len <= 9 {
            return Err(format!(
                "Piece message payload must contain data after the 8-byte header, got {} bytes",
                len.saturating_sub(1)
            ));
        }
        let index = u32::from_be_bytes([data[5], data[6], data[7], data[8]]);
        let begin = u32::from_be_bytes([data[9], data[10], data[11], data[12]]);
        return Ok(Some(BtMessage::Piece {
            index,
            begin,
            data: data.slice(13..4 + len),
        }));
    }

    parse_message(&data)
}

fn parse_port(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() != 2 {
        return Err(format!(
            "Port message payload must be 2 bytes, got {}",
            payload.len()
        ));
    }
    let port = u16::from_be_bytes([payload[0], payload[1]]);
    Ok(Some(BtMessage::Port { port }))
}

fn parse_allowed_fast(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() != 4 {
        return Err(format!(
            "AllowedFast message payload must be 4 bytes, got {}",
            payload.len()
        ));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::AllowedFast { index }))
}

fn parse_suggest(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() != 4 {
        return Err(format!(
            "Suggest message payload must be 4 bytes, got {}",
            payload.len()
        ));
    }
    let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Some(BtMessage::Suggest { index }))
}

fn parse_reject(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.len() != 12 {
        return Err(format!(
            "Reject message payload must be 12 bytes, got {}",
            payload.len()
        ));
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

/// Parse BEP 10 Extended message (ID=20).
///
/// Wire format: `<len><0x14><ext_id><bencoded payload>`
/// - `ext_id` = 0: extension handshake (dict of supported extensions)
/// - `ext_id` = 1+: application-specific (negotiated during handshake)
fn parse_extended(payload: &[u8]) -> Result<Option<BtMessage>, String> {
    if payload.is_empty() {
        return Err(
            "Extended message payload too short: 0 bytes (need at least ext_id)".to_string(),
        );
    }
    let ext_id = payload[0];
    let ext_payload = payload[1..].to_vec();
    Ok(Some(BtMessage::Extended {
        ext_id,
        payload: ext_payload,
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
                tracing::warn!("Failed to parse message: {}, skipping", e);
                break;
            }
        }
        pos += total_msg_size;
    }
    results
}

#[cfg(test)]
mod tests;
