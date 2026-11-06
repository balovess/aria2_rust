use sha1::Digest;

use crate::error::Result;

/// Verify a downloaded piece without running the digest on a Tokio worker.
///
/// The owned payload is returned so callers can write it after verification
/// without allocating a second piece-sized buffer.
pub(super) async fn verify_piece_hash_async(
    expected: Option<crate::engine::bittorrent::piece::PieceVerification>,
    data: Vec<u8>,
) -> Result<(bool, Vec<u8>)> {
    let Some(expected) = expected else {
        return Ok((false, data));
    };
    crate::checksum::hash_worker_pool::shared()
        .run(
            move || {
                Ok({
                    let verified = match expected {
                        crate::engine::bittorrent::piece::PieceVerification::Sha1(hashes) => hashes
                            .first()
                            .is_some_and(|hash| sha1::Sha1::digest(&data).as_slice() == hash),
                        crate::engine::bittorrent::piece::PieceVerification::V2 {
                            piece_length,
                            hashes,
                        } => hashes
                            .first()
                            .zip(aria2_protocol::bittorrent::torrent::merkle::piece_root(
                                &data,
                                piece_length as usize,
                            ))
                            .is_some_and(|(expected, actual)| expected.as_ref() == Some(&actual)),
                        crate::engine::bittorrent::piece::PieceVerification::Hybrid {
                            piece_length,
                            sha1,
                            v2_hashes,
                            v2_content_lengths,
                        } => {
                            let sha1_ok = sha1
                                .first()
                                .is_some_and(|hash| sha1::Sha1::digest(&data).as_slice() == hash);
                            let v2_ok = match v2_hashes.first().and_then(Option::as_ref) {
                                Some(expected) => v2_content_lengths
                                    .first()
                                    .copied()
                                    .filter(|length| *length != 0)
                                    .and_then(|length| data.get(..length as usize))
                                    .and_then(|content| {
                                        aria2_protocol::bittorrent::torrent::merkle::piece_root(
                                            content,
                                            piece_length as usize,
                                        )
                                    })
                                    .is_some_and(|actual| expected == &actual),
                                None => true,
                            };
                            sha1_ok && v2_ok
                        }
                    };
                    (verified, data)
                })
            },
            "piece hash",
        )
        .await
}
