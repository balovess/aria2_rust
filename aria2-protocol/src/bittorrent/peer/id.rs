use std::sync::OnceLock;

use rand::RngCore;

/// Return the process-wide peer ID, matching aria2's static peer identity.
pub fn generate_peer_id() -> [u8; 20] {
    static PEER_ID: OnceLock<[u8; 20]> = OnceLock::new();
    *PEER_ID.get_or_init(|| generate_peer_id_with_prefix(crate::identity::DEFAULT_PEER_ID_PREFIX))
}

pub fn generate_peer_id_with_prefix(prefix: &str) -> [u8; 20] {
    let mut id = [0u8; 20];
    let prefix_bytes = prefix.as_bytes();
    let copy_len = prefix_bytes.len().min(id.len());
    id[..copy_len].copy_from_slice(&prefix_bytes[..copy_len]);
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut id[copy_len..]);
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
        assert!(id.starts_with(crate::identity::DEFAULT_PEER_ID_PREFIX.as_bytes()));
        assert!(is_valid_peer_id(&id));
    }

    #[test]
    fn test_generate_peer_id_is_static_per_process() {
        let id1 = generate_peer_id();
        let id2 = generate_peer_id();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_custom_prefix() {
        let id = generate_peer_id_with_prefix("A2-1-37-0-");
        assert!(id.starts_with(b"A2-1-37-0-"));
        assert!(is_valid_peer_id(&id));
    }

    #[test]
    fn test_invalid_peer_id() {
        assert!(!is_valid_peer_id(&[]));
        assert!(!is_valid_peer_id(&[0u8; 19]));
        assert!(!is_valid_peer_id(&[0u8; 21]));
    }
}
