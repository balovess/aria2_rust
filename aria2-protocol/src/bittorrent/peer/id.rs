use rand::Rng;

/// Peer ID prefix for aria2-rust following BEP 20 format
/// Format: -AR2rs-XXXXXX (where XXXXXX is random alphanumeric)
/// This identifies the client as aria2-rust implementation
const PEER_ID_PREFIX: &[u8] = b"-AR2rs-";

pub fn generate_peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..7].copy_from_slice(PEER_ID_PREFIX);
    // Pad to 8 characters if needed (BEP 20 requires 8-char prefix)
    id[7] = b'-';
    let mut rng = rand::thread_rng();
    // Fill remaining 12 bytes with random alphanumeric characters
    for slot in &mut id[8..] {
        // Use alphanumeric characters (0-9, A-Z, a-z)
        let charset: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        *slot = charset[rng.gen_range(0..charset.len())];
    }
    id
}

pub fn generate_peer_id_with_prefix(prefix: &str) -> [u8; 20] {
    let mut id = [0u8; 20];
    let prefix_bytes = prefix.as_bytes();
    let copy_len = prefix_bytes.len().min(8);
    id[..copy_len].copy_from_slice(&prefix_bytes[..copy_len]);
    for slot in &mut id[copy_len..8] {
        *slot = b'-';
    }
    let mut rng = rand::thread_rng();
    for slot in &mut id[8..] {
        *slot = rng.gen_range(b'0'..=b'9');
    }
    id
}

pub fn is_valid_peer_id(peer_id: &[u8]) -> bool {
    peer_id.len() == 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_peer_id() {
        let id = generate_peer_id();
        assert_eq!(id.len(), 20);
        assert!(id.starts_with(PEER_ID_PREFIX));
        assert!(is_valid_peer_id(&id));
    }

    #[test]
    fn test_generate_peer_id_uniqueness() {
        let id1 = generate_peer_id();
        let id2 = generate_peer_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_custom_prefix() {
        let id = generate_peer_id_with_prefix("-UT3460-");
        assert!(id.starts_with(b"-UT3460-"));
        assert!(is_valid_peer_id(&id));
    }

    #[test]
    fn test_invalid_peer_id() {
        assert!(!is_valid_peer_id(&[]));
        assert!(!is_valid_peer_id(&[0u8; 19]));
        assert!(!is_valid_peer_id(&[0u8; 21]));
    }
}
