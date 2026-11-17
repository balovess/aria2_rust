//! Single-mirror payload transfer and staging.

use std::path::Path;

use super::{MetalinkDownloadCommand, PayloadDownload};
use crate::error::{Aria2Error, RecoverableError, Result};

impl MetalinkDownloadCommand {
    pub(super) async fn download_payload_url(
        &self,
        output_path: &Path,
        url: &str,
        expected_size: Option<u64>,
        source_index: usize,
    ) -> Result<PayloadDownload> {
        let stage_path = self.stage_path(output_path, source_index);
        self.download_source_to_path(url, &stage_path).await?;

        let completed_length = tokio::fs::metadata(&stage_path)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?
            .len();
        if let Some(expected) = expected_size
            && completed_length != expected
        {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Metalink size mismatch: expected {expected} bytes, received {completed_length}"
                    ),
                },
            ));
        }

        Ok(PayloadDownload {
            path: stage_path,
            completed_length,
            total_length: expected_size.unwrap_or(completed_length),
        })
    }
}
