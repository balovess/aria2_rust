use std::path::{Component, Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::super::listing::parse_ftp_list_response;
use super::{DownloadProgress, DownloadResult, FtpDownload};

pub(super) fn local_child_path(base: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(base.join(name)),
        _ => Err(format!("Unsafe FTP listing entry name: {name:?}")),
    }
}

impl FtpDownload<'_> {
    /// Download a directory recursively when recursive download is enabled.
    pub async fn download_directory(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
    ) -> Result<Vec<DownloadResult>, String> {
        self.download_directory_controlled(remote_dir, local_base_dir, progress_callback, None)
            .await
    }

    /// Download a directory recursively while observing a cancellation token.
    pub async fn download_directory_with_cancellation(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<DownloadResult>, String> {
        self.download_directory_controlled(
            remote_dir,
            local_base_dir,
            progress_callback,
            Some(cancellation),
        )
        .await
    }

    async fn download_directory_controlled(
        &mut self,
        remote_dir: &str,
        local_base_dir: &str,
        progress_callback: Option<fn(DownloadProgress)>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Vec<DownloadResult>, String> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("FTP download cancelled".to_string());
        }

        if !self.options.recursive_download {
            return Err("Recursive download not enabled in options".to_string());
        }

        self.conn.cwd(remote_dir).await?;

        // Negotiate the channel before LIST; the server opens it after the
        // transfer command arrives.
        let data_connection = self.establish_data_connection().await?;
        let list_response = self.conn.list(None).await?;
        if !list_response.is_positive_preliminary() {
            return Err(format!(
                "LIST failed: {} {}",
                list_response.code, list_response.message
            ));
        }

        let listing_data = self
            .receive_data_to_memory(data_connection, None, cancellation)
            .await?;
        let listing = String::from_utf8_lossy(&listing_data);
        let entries = parse_ftp_list_response(&listing);

        std::fs::create_dir_all(local_base_dir)
            .map_err(|error| format!("Failed to create local directory: {error}"))?;

        let mut results = Vec::new();
        for entry in entries {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err("FTP download cancelled".to_string());
            }

            if entry.is_directory {
                let sub_remote = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let sub_local = local_child_path(Path::new(local_base_dir), &entry.name)?
                    .to_string_lossy()
                    .into_owned();
                let sub_results = Box::pin(self.download_directory_controlled(
                    &sub_remote,
                    &sub_local,
                    progress_callback,
                    cancellation,
                ))
                .await?;
                results.extend(sub_results);
            } else {
                let remote_file = format!("{}/{}", remote_dir.trim_end_matches('/'), entry.name);
                let local_file = local_child_path(Path::new(local_base_dir), &entry.name)?
                    .to_string_lossy()
                    .into_owned();
                let result = self
                    .download_file_controlled(
                        &remote_file,
                        &local_file,
                        progress_callback,
                        cancellation,
                    )
                    .await?;
                results.push(result);
            }
        }

        Ok(results)
    }
}
