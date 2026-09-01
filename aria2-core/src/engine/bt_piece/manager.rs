use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
use sha1::Digest;

#[derive(Clone, Debug)]
pub enum PieceVerification {
    Sha1(Vec<[u8; 20]>),
    /// BEP 52 piece-layer roots indexed by torrent piece.
    V2 {
        piece_length: u32,
        hashes: Vec<Option<[u8; 32]>>,
    },
    /// Hybrid torrents must validate the same piece against both BEP 3 and
    /// BEP 52 hashes before it can be written or announced.
    Hybrid {
        piece_length: u32,
        sha1: Vec<[u8; 20]>,
        v2_hashes: Vec<Option<[u8; 32]>>,
        /// Content lengths indexed by piece; zero means no v2 hash/content.
        /// Valid torrent file lengths are positive and bounded by piece_length.
        v2_content_lengths: Vec<u32>,
    },
}

pub struct PieceManager {
    num_pieces: u32,
    piece_length: u32,
    total_size: u64,
    completed: Bitfield,
    total_downloaded: u64,
    verification: PieceVerification,
}

impl PieceManager {
    pub fn new(num_pieces: u32, piece_length: u32, total_size: u64, hashes: &[[u8; 20]]) -> Self {
        assert_eq!(num_pieces as usize, hashes.len());
        Self::new_owned(num_pieces, piece_length, total_size, hashes.to_vec())
    }

    /// Construct a manager by taking ownership of v1 piece hashes.
    ///
    /// The owned form lets torrent execution transfer parsed metadata directly
    /// instead of retaining a second copy for the download lifetime.
    pub fn new_owned(
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
        hashes: Vec<[u8; 20]>,
    ) -> Self {
        assert_eq!(num_pieces as usize, hashes.len());
        Self {
            num_pieces,
            piece_length,
            total_size,
            completed: Bitfield::new(num_pieces as usize),
            total_downloaded: 0,
            verification: PieceVerification::Sha1(hashes),
        }
    }

    pub fn new_v2(
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
        files: Vec<(u64, Vec<[u8; 32]>)>,
    ) -> Self {
        assert!(piece_length >= aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE as u32);
        Self {
            num_pieces,
            piece_length,
            total_size,
            completed: Bitfield::new(num_pieces as usize),
            total_downloaded: 0,
            verification: PieceVerification::V2 {
                piece_length,
                hashes: Self::build_v2_hashes(piece_length, files).0,
            },
        }
    }

    pub fn new_hybrid(
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
        sha1: &[[u8; 20]],
        files: Vec<(u64, Vec<[u8; 32]>)>,
    ) -> Self {
        assert_eq!(num_pieces as usize, sha1.len());
        Self::new_hybrid_owned(
            num_pieces,
            piece_length,
            total_size,
            sha1.to_vec(),
            files,
        )
    }

    /// Construct a hybrid manager by taking ownership of v1 piece hashes.
    pub fn new_hybrid_owned(
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
        sha1: Vec<[u8; 20]>,
        files: Vec<(u64, Vec<[u8; 32]>)>,
    ) -> Self {
        assert_eq!(num_pieces as usize, sha1.len());
        assert!(piece_length >= aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE as u32);
        let (v2_hashes, v2_content_lengths) = Self::build_v2_hashes(piece_length, files);
        Self {
            num_pieces,
            piece_length,
            total_size,
            completed: Bitfield::new(num_pieces as usize),
            total_downloaded: 0,
            verification: PieceVerification::Hybrid {
                piece_length,
                sha1,
                v2_hashes,
                v2_content_lengths,
            },
        }
    }

    fn build_v2_hashes(
        piece_length: u32,
        files: Vec<(u64, Vec<[u8; 32]>)>,
    ) -> (Vec<Option<[u8; 32]>>, Vec<u32>) {
        let mut result = Vec::new();
        let mut content_lengths = Vec::new();
        let mut address = 0u64;
        for (length, hashes) in files {
            if length == 0 {
                continue;
            }
            let start_piece = address.div_ceil(piece_length as u64) as usize;
            result.resize(result.len().max(start_piece), None);
            content_lengths.resize(content_lengths.len().max(start_piece), 0);
            let count = length.div_ceil(piece_length as u64) as usize;
            result.extend(hashes.into_iter().take(count).map(Some));
            content_lengths.extend((0..count).map(|index| {
                u32::try_from(
                    (length - index as u64 * piece_length as u64)
                        .min(piece_length as u64),
                )
                    .expect("v2 piece content length must fit in u32")
            }));
            address = start_piece as u64 * piece_length as u64 + length;
        }
        (result, content_lengths)
    }

    pub fn piece_size(&self, index: u32) -> u32 {
        if index >= self.num_pieces - 1 {
            let remainder = self.total_size % self.piece_length as u64;
            if remainder > 0 {
                remainder as u32
            } else {
                self.piece_length
            }
        } else {
            self.piece_length
        }
    }

    pub fn is_completed(&self, index: u32) -> bool {
        self.completed.test(index as usize)
    }

    pub fn mark_piece_downloaded(&mut self, index: u32, bytes: u64) {
        if (index as usize) < self.num_pieces as usize {
            self.total_downloaded += bytes;
        }
    }

    pub fn mark_piece_complete(&mut self, index: u32) {
        self.completed.set(index as usize);
    }

    pub fn verify_piece_hash(&self, index: u32, data: &[u8]) -> bool {
        match &self.verification {
            PieceVerification::Sha1(hashes) => {
                let Some(expected) = hashes.get(index as usize) else {
                    return false;
                };
                sha1::Sha1::digest(data).as_slice() == expected
            }
            PieceVerification::V2 {
                piece_length,
                hashes,
            } => {
                let Some(Some(expected)) = hashes.get(index as usize) else {
                    return false;
                };
                aria2_protocol::bittorrent::torrent::merkle::piece_root(
                    data,
                    *piece_length as usize,
                )
                .is_some_and(|actual| actual == *expected)
            }
            PieceVerification::Hybrid {
                piece_length,
                sha1,
                v2_hashes,
                v2_content_lengths,
            } => {
                let Some(sha1_expected) = sha1.get(index as usize) else {
                    return false;
                };
                if sha1::Sha1::digest(data).as_slice() != sha1_expected {
                    return false;
                }
                match v2_hashes.get(index as usize).and_then(Option::as_ref) {
                    Some(expected) => {
                        let Some(&content_length) = v2_content_lengths.get(index as usize) else {
                            return false;
                        };
                        if content_length == 0 {
                            return false;
                        }
                        let Some(content) = data.get(..content_length as usize) else {
                            return false;
                        };
                        aria2_protocol::bittorrent::torrent::merkle::piece_root(
                            content,
                            *piece_length as usize,
                        )
                        .is_some_and(|actual| actual == *expected)
                    }
                    None => true,
                }
            }
        }
    }

    /// Return the expected hash by value so a caller can verify on another
    /// thread without borrowing the piece manager across an await point.
    pub fn expected_piece_verification(&self, index: u32) -> Option<PieceVerification> {
        match &self.verification {
            PieceVerification::Sha1(hashes) => hashes
                .get(index as usize)
                .map(|hash| PieceVerification::Sha1(vec![*hash])),
            PieceVerification::V2 {
                piece_length,
                hashes,
            } => hashes
                .get(index as usize)
                .and_then(|expected| expected.as_ref())
                .map(|expected| PieceVerification::V2 {
                    piece_length: *piece_length,
                    hashes: vec![Some(*expected)],
                }),
            PieceVerification::Hybrid {
                piece_length,
                sha1,
                v2_hashes,
                v2_content_lengths,
            } => sha1
                .get(index as usize)
                .map(|sha1_hash| PieceVerification::Hybrid {
                    piece_length: *piece_length,
                    sha1: vec![*sha1_hash],
                    v2_hashes: vec![v2_hashes.get(index as usize).copied().flatten()],
                    v2_content_lengths: vec![
                        v2_content_lengths.get(index as usize).copied().unwrap_or(0),
                    ],
                }),
        }
    }

    pub fn completed_pieces(&self) -> u32 {
        self.completed.count_set() as u32
    }

    pub fn total_progress(&self) -> f64 {
        if self.total_size == 0 {
            return 100.0;
        }
        self.total_downloaded as f64 / self.total_size as f64 * 100.0
    }

    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }
    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }
    pub fn total_size(&self) -> u64 {
        self.total_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let hashes: Vec<[u8; 20]> = (0..3).map(|_| [0u8; 20]).collect();
        let mgr = PieceManager::new(3, 512, 1024, &hashes);
        assert_eq!(mgr.num_pieces(), 3);
        assert_eq!(mgr.piece_length(), 512);
        assert_eq!(mgr.total_size(), 1024);
    }

    #[test]
    fn test_last_piece_size() {
        let hashes: Vec<[u8; 20]> = (0..3).map(|_| [0u8; 20]).collect();
        let mgr = PieceManager::new(3, 512, 1100, &hashes);
        assert_eq!(mgr.piece_size(0), 512);
        assert_eq!(mgr.piece_size(1), 512);
        assert_eq!(mgr.piece_size(2), 76);
    }

    #[test]
    fn test_mark_and_verify() {
        let hashes: Vec<[u8; 20]> = (0..2).map(|_| [0u8; 20]).collect();
        let mut mgr = PieceManager::new(2, 100, 150, &hashes);
        assert!(!mgr.is_completed(0));

        mgr.mark_piece_downloaded(0, 50);
        assert!(!mgr.is_completed(0));

        mgr.mark_piece_complete(0);
        assert!(mgr.is_completed(0));
        assert_eq!(mgr.completed_pieces(), 1);

        mgr.mark_piece_complete(1);
        assert_eq!(mgr.completed_pieces(), 2);
    }

    #[test]
    fn test_total_progress() {
        let hashes: Vec<[u8; 20]> = (0..4).map(|_| [0u8; 20]).collect();
        let mut mgr = PieceManager::new(4, 256, 800, &hashes);
        assert_eq!(mgr.total_progress(), 0.0);

        mgr.mark_piece_downloaded(0, 256);
        mgr.mark_piece_downloaded(1, 256);
        assert!((mgr.total_progress() - 64.0).abs() < 0.01);
    }

    #[test]
    fn v2_multifile_piece_roots_skip_alignment_gaps() {
        let piece_length = aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE as u32;
        let first = vec![7u8];
        let second = vec![9u8; piece_length as usize];
        let first_root =
            aria2_protocol::bittorrent::torrent::merkle::piece_root(&first, piece_length as usize)
                .unwrap();
        let second_root =
            aria2_protocol::bittorrent::torrent::merkle::piece_root(&second, piece_length as usize)
                .unwrap();
        let manager = PieceManager::new_v2(
            2,
            piece_length,
            (first.len() + second.len()) as u64,
            vec![
                (first.len() as u64, vec![first_root]),
                (second.len() as u64, vec![second_root]),
            ],
        );

        assert!(manager.verify_piece_hash(0, &first));
        assert!(manager.verify_piece_hash(1, &second));
        assert!(!manager.verify_piece_hash(
            1,
            &[8u8; aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE]
        ));
    }

    #[test]
    fn hybrid_requires_both_sha1_and_merkle_hashes() {
        let data = vec![0x4au8; aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE];
        let sha1_hash: [u8; 20] = sha1::Sha1::digest(&data).into();
        let v2_hash = aria2_protocol::bittorrent::torrent::merkle::piece_root(
            &data,
            aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE,
        )
        .unwrap();
        let manager = PieceManager::new_hybrid(
            1,
            aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE as u32,
            data.len() as u64,
            &[sha1_hash],
            vec![(data.len() as u64, vec![v2_hash])],
        );
        assert!(manager.verify_piece_hash(0, &data));

        let mut wrong_v2 = v2_hash;
        wrong_v2[0] ^= 1;
        let manager = PieceManager::new_hybrid(
            1,
            aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE as u32,
            data.len() as u64,
            &[sha1_hash],
            vec![(data.len() as u64, vec![wrong_v2])],
        );
        assert!(!manager.verify_piece_hash(0, &data));
    }

    #[test]
    fn hybrid_merkle_verification_excludes_alignment_padding() {
        let piece_length = aria2_protocol::bittorrent::torrent::merkle::BLOCK_SIZE;
        let content = [0x5au8];
        let mut wire_piece = vec![0u8; piece_length];
        wire_piece[0] = content[0];
        let sha1_hash: [u8; 20] = sha1::Sha1::digest(&wire_piece).into();
        let v2_root =
            aria2_protocol::bittorrent::torrent::merkle::piece_root(&content, piece_length)
                .unwrap();
        let manager = PieceManager::new_hybrid(
            1,
            piece_length as u32,
            wire_piece.len() as u64,
            &[sha1_hash],
            vec![(content.len() as u64, vec![v2_root])],
        );

        assert!(manager.verify_piece_hash(0, &wire_piece));
    }
}
