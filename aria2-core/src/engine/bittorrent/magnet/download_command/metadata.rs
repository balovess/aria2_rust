//! Cached, normalized, and validated BitTorrent metadata for magnet downloads.

use super::MagnetDownloadCommand;
use std::io::Write;
use tracing::{info, warn};
impl MagnetDownloadCommand {
    pub(super) fn saved_metadata_path(&self, info_hash: &[u8; 20]) -> std::path::PathBuf {
        self.output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}.torrent", hex::encode(info_hash)))
    }

    pub(super) fn saved_metadata_path_for_magnet(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> std::path::PathBuf {
        let name = magnet
            .info_hash_v2
            .map(hex::encode)
            .unwrap_or_else(|| hex::encode(magnet.info_hash));
        self.output_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{}.torrent", name))
    }

    pub(super) fn load_saved_metadata(&self, info_hash: &[u8; 20]) -> Option<Vec<u8>> {
        let path = self.saved_metadata_path(info_hash);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(path = %path.display(), %error, "Failed to read saved BitTorrent metadata");
                return None;
            }
        };

        match aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&data) {
            Ok(meta) if meta.info_hash.bytes == *info_hash => {
                info!(path = %path.display(), "Loaded BitTorrent metadata from saved torrent file");
                Some(data)
            }
            Ok(meta) => {
                warn!(
                    path = %path.display(),
                    actual_info_hash = %meta.info_hash.as_hex(),
                    expected_info_hash = %hex::encode(info_hash),
                    "Ignoring saved BitTorrent metadata with unexpected info-hash"
                );
                None
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "Ignoring invalid saved BitTorrent metadata");
                None
            }
        }
    }

    pub(super) fn load_saved_metadata_for_magnet(
        &self,
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
    ) -> Option<Vec<u8>> {
        if magnet.info_hash_v2.is_none() {
            return self.load_saved_metadata(&magnet.info_hash);
        }
        let path = self.saved_metadata_path_for_magnet(magnet);
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(path = %path.display(), %error, "Failed to read saved BitTorrent metadata");
                return None;
            }
        };
        match Self::metadata_matches_magnet(magnet, &data) {
            Ok(()) => {
                info!(path = %path.display(), "Loaded BitTorrent metadata from saved torrent file");
                Some(data)
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "Ignoring saved BitTorrent metadata with unexpected info-hash");
                None
            }
        }
    }

    pub(super) fn metadata_matches_magnet(
        magnet: &aria2_protocol::bittorrent::magnet::MagnetLink,
        torrent_bytes: &[u8],
    ) -> std::result::Result<(), String> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)?;
        if let Some(expected_v1) = magnet.info_hash_v1
            && meta.info_hash.bytes != expected_v1
        {
            return Err(format!(
                "v1 info-hash mismatch: expected {}, got {}",
                hex::encode(expected_v1),
                meta.info_hash.as_hex()
            ));
        }
        if let Some(expected_v2) = magnet.info_hash_v2
            && meta.info_hash_v2 != Some(expected_v2)
        {
            return Err(format!(
                "v2 info-hash mismatch: expected {}, got {}",
                hex::encode(expected_v2),
                meta.info_hash_v2
                    .map(hex::encode)
                    .unwrap_or_else(|| "absent".into())
            ));
        }
        Ok(())
    }

    /// Fetch torrent metadata from BEP 9's `xs` exact-source parameter.
    ///
    /// An exact source is a complete `.torrent` file, so it is a cheaper and
    /// more deterministic metadata path than starting DHT and waiting for a
    /// metadata-capable peer. HTTP(S) sources use the same proxy, TLS, and
    /// authentication options as ordinary HTTP downloads. Local `file://`
    /// sources are also accepted and still go through the info-hash check.
    pub(super) fn save_metadata_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<bool> {
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error),
        };

        if let Err(error) = file.write_all(data).and_then(|_| file.flush()) {
            let _ = std::fs::remove_file(path);
            return Err(error);
        }
        Ok(true)
    }
}
