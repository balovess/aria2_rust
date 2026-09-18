//! Metadata retrieval and Metalink hash verification.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;
use tracing::warn;

use super::{MetalinkDownloadCommand, classify_metalink_http_status};
use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl MetalinkDownloadCommand {
    #[cfg(feature = "bittorrent")]
    pub(super) async fn download_metadata_url(&self, url: &str) -> Result<Vec<u8>> {
        if let Some(error) = self.lifecycle_error() {
            return Err(error);
        }

        let response = tokio::select! {
            response = self.client.get(url).send() => response.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("HTTP request failed: {e}"),
                })
            })?,
            _ = self.wait_for_lifecycle_change() => {
                return Err(self.lifecycle_error().unwrap_or_else(|| {
                    Aria2Error::DownloadFailed("Metalink metadata download halted".into())
                }));
            }
        };
        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            return Err(classify_metalink_http_status(status.as_u16()));
        }

        tokio::select! {
            bytes = response.bytes() => bytes
                .map(|bytes| bytes.to_vec())
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP metadata read failed: {e}"),
                    })
                }),
            _ = self.wait_for_lifecycle_change() => {
                Err(self.lifecycle_error().unwrap_or_else(|| {
                    Aria2Error::DownloadFailed("Metalink metadata download halted".into())
                }))
            }
        }
    }

    #[cfg(feature = "bittorrent")]
    pub(super) async fn download_metadata_url_with_retry(&self, url: &str) -> Result<Vec<u8>> {
        let options = self.group.recover().options_arc();
        let retry_policy =
            RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
        let mut attempts = 0u32;

        loop {
            match self.download_metadata_url(url).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    let error = self.record_not_found_error(error);
                    if self.lifecycle_error().is_some()
                        || self.should_stop_after_not_found(&error)
                        || !self.should_retry_mirror_error(attempts, &error, &retry_policy)
                    {
                        return Err(error);
                    }

                    attempts = attempts.saturating_add(1);
                    let wait = retry_policy.compute_wait(attempts).unwrap_or_default();
                    warn!(
                        url,
                        attempt = attempts.saturating_add(1),
                        max_attempts = retry_policy.max_tries(),
                        ?wait,
                        error = %error,
                        "Metalink torrent metaurl failed, retrying"
                    );
                    self.wait_for_retry(wait).await?;
                }
            }
        }
    }

    pub(super) async fn verify_file_hash(
        &self,
        path: &Path,
        total_length: u64,
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        let hash_type = HashType::from_str(hash.algo.as_standard_name())
            .ok_or_else(|| Aria2Error::Parse("unsupported Metalink hash algorithm".into()))?;
        let checksum = Checksum::new(hash_type, &hash.value)?;
        crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
            &crate::checksum::check_integrity::man::shared(),
            std::sync::Arc::clone(&self.group),
            path,
            total_length,
            checksum,
        )
        .await
    }

    /// Verify a whole-file download against Metalink `<pieces>` chunk hashes.
    ///
    /// Mirrors C++ `MetalinkEntry::checksum` / `ChunkChecksum` verification:
    /// the data is split into `pieces.length`-sized chunks and each chunk is
    /// compared against its corresponding digest. Returns `Ok(false)` on any
    /// mismatch, when the number of digests does not match the expected piece
    /// count, or when a digest has the wrong hex length.
    #[cfg(test)]
    pub(crate) fn verify_pieces(
        &self,
        data: &[u8],
        pieces: &aria2_protocol::metalink::parser::PieceInfo,
    ) -> Result<bool> {
        if pieces.hashes.is_empty() {
            return Ok(true);
        }
        let hex_len = pieces.type_.hash_len();
        if pieces.hashes.iter().any(|h| h.len() != hex_len) {
            warn!(
                algo = ?pieces.type_,
                "Metalink pieces digest length mismatch, verification failed"
            );
            return Ok(false);
        }

        let expected = pieces.num_pieces(data.len() as u64);
        if pieces.hashes.len() != expected {
            warn!(
                expected,
                actual = pieces.hashes.len(),
                "Metalink pieces count mismatch, verification failed"
            );
            return Ok(false);
        }

        let chunk_len = pieces.length as usize;
        for (i, expected_hash) in pieces.hashes.iter().enumerate() {
            let start = i * chunk_len;
            let end = ((i + 1) * chunk_len).min(data.len());
            let chunk = &data[start..end];
            let actual = digest_hex(chunk, pieces.type_);
            if !actual.eq_ignore_ascii_case(expected_hash) {
                warn!(
                    piece = i,
                    "Metalink piece hash mismatch ({} / {})",
                    i + 1,
                    pieces.hashes.len()
                );
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn verify_pieces_file(
        &self,
        path: &Path,
        pieces: &aria2_protocol::metalink::parser::PieceInfo,
    ) -> Result<bool> {
        if pieces.hashes.is_empty() {
            return Ok(true);
        }
        if pieces.length == 0 {
            return Ok(false);
        }
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
        let expected = pieces.num_pieces(metadata.len());
        if pieces.hashes.len() != expected
            || pieces
                .hashes
                .iter()
                .any(|hash| hash.len() != pieces.type_.hash_len())
        {
            return Ok(false);
        }

        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
        let mut buffer = vec![0u8; pieces.length as usize];
        for (index, expected_hash) in pieces.hashes.iter().enumerate() {
            let mut read = 0usize;
            while read < buffer.len() {
                let count = tokio::io::AsyncReadExt::read(&mut file, &mut buffer[read..]).await?;
                if count == 0 {
                    break;
                }
                read += count;
            }
            let (actual, returned_buffer) = digest_hex_async(buffer, read, pieces.type_).await?;
            buffer = returned_buffer;
            if !actual.eq_ignore_ascii_case(expected_hash) {
                warn!(piece = index, "Metalink piece hash mismatch");
                return Ok(false);
            }
            if read < buffer.len() && index + 1 < pieces.hashes.len() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Compute the lowercase hex digest of `data` for a Metalink hash algorithm.
fn digest_hex(data: &[u8], algo: aria2_protocol::metalink::parser::HashAlgorithm) -> String {
    use aria2_protocol::metalink::parser::HashAlgorithm;
    match algo {
        HashAlgorithm::Md5 => {
            use md5::Digest;
            let mut hasher = md5::Md5::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha1 => {
            use sha1::Digest;
            let mut hasher = sha1::Sha1::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha224 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha224::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha256 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha384 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha384::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha512 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha512::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
    }
}

const MAX_METALINK_HASH_WORKERS: usize = 4;

fn metalink_hash_slots() -> &'static Arc<Semaphore> {
    static HASH_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    HASH_SLOTS.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_METALINK_HASH_WORKERS);
        Arc::new(Semaphore::new(workers))
    })
}

async fn digest_hex_async(
    data: Vec<u8>,
    used: usize,
    algo: aria2_protocol::metalink::parser::HashAlgorithm,
) -> Result<(String, Vec<u8>)> {
    debug_assert!(used <= data.len());
    let permit = metalink_hash_slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| Aria2Error::Io(format!("Metalink hash dispatcher closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let digest = digest_hex(&data[..used], algo);
        (digest, data)
    })
    .await
    .map_err(|error| Aria2Error::Io(format!("Metalink hash task failed: {error}")))
}
