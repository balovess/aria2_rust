//! BitTorrent v2 Merkle tree primitives (BEP 52).

use sha2::{Digest, Sha256};

pub const BLOCK_SIZE: usize = 16 * 1024;

pub fn hash_block(block: &[u8]) -> [u8; 32] {
    Sha256::digest(block).into()
}

pub fn parent_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(left);
    input[32..].copy_from_slice(right);
    Sha256::digest(input).into()
}

/// Compute a BEP 52 file root from complete file bytes.
pub fn file_root(data: &[u8]) -> [u8; 32] {
    if data.is_empty() {
        return [0u8; 32];
    }
    let mut leaves: Vec<[u8; 32]> = data.chunks(BLOCK_SIZE).map(hash_block).collect();
    let leaf_count = leaves.len().next_power_of_two();
    leaves.resize(leaf_count, [0u8; 32]);
    reduce_tree(leaves)
}

/// Compute the root from a piece layer. The layer must contain complete 16 KiB
/// block-subtree roots and is padded with the empty-block hash as BEP 52 does.
pub fn root_from_piece_layer(piece_layer: &[[u8; 32]]) -> Option<[u8; 32]> {
    if piece_layer.is_empty() {
        return None;
    }
    let target_len = piece_layer.len().next_power_of_two();
    let mut hashes = piece_layer.to_vec();
    hashes.resize(target_len, [0u8; 32]);
    Some(reduce_tree(hashes))
}

pub fn verify_piece_layer(root: &[u8; 32], piece_layer: &[[u8; 32]]) -> bool {
    root_from_piece_layer(piece_layer).is_some_and(|actual| &actual == root)
}

/// Compute the BEP 52 subtree root for one torrent piece. Missing blocks in
/// the final piece are represented by the empty-block hash.
pub fn piece_root(data: &[u8], piece_length: usize) -> Option<[u8; 32]> {
    if piece_length < BLOCK_SIZE || !piece_length.is_multiple_of(BLOCK_SIZE) {
        return None;
    }
    if data.len() > piece_length {
        return None;
    }
    let block_count = piece_length / BLOCK_SIZE;
    if !block_count.is_power_of_two() {
        return None;
    }
    let mut leaves: Vec<_> = data.chunks(BLOCK_SIZE).map(hash_block).collect();
    leaves.resize(block_count, [0u8; 32]);
    Some(reduce_tree(leaves))
}

fn reduce_tree(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    while hashes.len() > 1 {
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| parent_hash(&pair[0], &pair[1]))
            .collect();
    }
    hashes[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_has_stable_root() {
        assert_eq!(file_root(&[]), [0u8; 32]);
    }

    #[test]
    fn file_root_changes_when_data_changes() {
        assert_ne!(file_root(b"a"), file_root(b"b"));
    }

    #[test]
    fn piece_layer_verification_rejects_tampering() {
        let layer = [hash_block(b"piece-a"), hash_block(b"piece-b")];
        let root = root_from_piece_layer(&layer).unwrap();
        assert!(verify_piece_layer(&root, &layer));

        let mut changed = layer;
        changed[1] = hash_block(b"corrupted");
        assert!(!verify_piece_layer(&root, &changed));
    }

    #[test]
    fn non_power_of_two_layers_are_padded_deterministically() {
        let layer = [hash_block(b"a"), hash_block(b"b"), hash_block(b"c")];
        let root = root_from_piece_layer(&layer).unwrap();
        assert!(verify_piece_layer(&root, &layer));
        assert!(!verify_piece_layer(&root, &layer[..2]));
    }

    #[test]
    fn piece_root_rejects_corruption_and_pads_final_piece() {
        let data = vec![7u8; BLOCK_SIZE + 3];
        let root = piece_root(&data, BLOCK_SIZE * 2).unwrap();
        assert_eq!(root, piece_root(&data, BLOCK_SIZE * 2).unwrap());
        assert_ne!(
            root,
            piece_root(&[8u8; BLOCK_SIZE + 3], BLOCK_SIZE * 2).unwrap()
        );
        assert!(piece_root(&vec![0u8; BLOCK_SIZE * 2 + 1], BLOCK_SIZE * 2).is_none());
    }
}
