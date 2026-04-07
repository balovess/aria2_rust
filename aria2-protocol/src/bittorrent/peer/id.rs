use rand::Rng;

const PEER_ID_PREFIX: &[u8] = b"-AR0001-";

pub fn generate_peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(PEER_ID_PREFIX);
    let mut rng = rand::thread_rng();
    for i in 8..20 {
        id[i] = rng.gen_range(b'A'..=b'Z');
    }
    id
}

pub fn generate_peer_id_with_prefix(prefix: &str) -> [u8; 20] {
    let mut id = [0u8; 20];
    let prefix_bytes = prefix.as_bytes();
    let copy_len = prefix_bytes.len().min(8);
    id[..copy_len].copy_from_slice(&prefix_bytes[..copy_len]);
    if copy_len < 8 {
        for i in copy_len..8 { id[i] = b'-'; }
    }
    let mut rng = rand::thread_rng();
    for i in 8..20 {
        id[i] = rng.gen_range(b'0'..=b'9');
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
