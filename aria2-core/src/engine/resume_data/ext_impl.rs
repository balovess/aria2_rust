//! ResumeDataExt trait implementation for ResumeData
//!
//! Contains the concrete implementation of from_request_group and
//! to_restore_components for converting between ResumeData and RequestGroup.

use super::ext_trait::ResumeDataExt;
use super::types::{ChecksumInfo, MirrorRestoreInfo, RestoreState, ResumeData, UriState};
use crate::request::request_group::{DownloadStatus, RequestGroup};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

#[cfg(feature = "metalink")]
use base64::Engine;

impl ResumeDataExt for ResumeData {
    fn from_request_group(group: &RequestGroup) -> Result<Self, String> {
        // Extract identity
        let gid = group.gid().to_hex_string();

        // Extract URIs with state tracking
        let raw_uris = group.uris().to_vec();
        let uris: Vec<UriState> = raw_uris
            .iter()
            .map(|uri| UriState {
                uri: uri.clone(),
                tried: true, // Assume all added URIs were at least considered
                used: false, // Not actively connected at snapshot time
                last_result: None,
                speed_bytes_per_sec: None,
            })
            .collect();

        // Extract progress using lock-free atomics (preferred for frequent polling)
        let total_length = group.get_total_length_atomic();
        let completed_length = group.get_completed_length();
        let uploaded_length = group.get_uploaded_length();

        // Extract status (requires lock)
        let dl_status = group.status();
        let status_str = match dl_status {
            DownloadStatus::Active => "active",
            DownloadStatus::Waiting => "waiting",
            DownloadStatus::Paused => "paused",
            DownloadStatus::Complete => "complete",
            DownloadStatus::Removed => "removed",
            DownloadStatus::Error(ref err) => {
                // Include error context in the status field
                return Err(format!(
                    "Download in error state: {}. Error: {:?}",
                    gid, err
                ));
            }
        }
        .to_string();

        let error_message = match &dl_status {
            DownloadStatus::Error(err) => Some(format!("{:?}", err)),
            _ => None,
        };

        // Extract timing information
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last_download_time = created_at; // Simplified; real impl would track this

        // Extract file info from options
        let options = group.options();
        let output_path = options.out.clone().or_else(|| {
            // Construct path from dir + out if both exist
            options.dir.as_ref().and_then(|dir| {
                options.out.as_ref().map(|out| {
                    let mut p = dir.clone();
                    if !p.ends_with('/') && !p.ends_with('\\') {
                        p.push(std::path::MAIN_SEPARATOR);
                    }
                    p.push_str(out);
                    p
                })
            })
        });

        // Extract checksum if configured
        let checksum = options
            .checksum
            .as_ref()
            .map(|(algo, expected)| ChecksumInfo {
                algorithm: algo.clone(),
                expected: expected.clone(),
            });

        // Extract download options subset for persistence
        let mut options_map = HashMap::new();
        if let Some(split) = options.split {
            options_map.insert("split".to_string(), split.to_string());
        }
        if let Some(mcps) = options.max_connection_per_server {
            options_map.insert("max_connection_per-server".to_string(), mcps.to_string());
        }
        if let Some(ref dir) = options.dir {
            options_map.insert("dir".to_string(), dir.clone());
        }
        if let Some(ref out) = options.out {
            options_map.insert("out".to_string(), out.clone());
        }
        if let Some(mode) = options.follow_torrent {
            options_map.insert("follow-torrent".to_string(), mode.as_str().to_string());
        }
        if let Some(mode) = options.follow_metalink {
            options_map.insert("follow-metalink".to_string(), mode.as_str().to_string());
        }
        if let Some(seed_time) = options.seed_time {
            options_map.insert("seed-time".to_string(), seed_time.to_string());
        }
        if let Some(seed_ratio) = options.seed_ratio {
            options_map.insert("seed-ratio".to_string(), seed_ratio.to_string());
        }

        // Extract BT-specific fields
        let bt_bitfield = group.get_bt_bitfield();
        let metadata_info = group.metadata_info();
        #[cfg(feature = "metalink")]
        let metalink_source = group.metalink_source();

        // Determine if this is a BT download from URI pattern or resolved
        // metadata provenance.
        let is_bt = raw_uris.iter().any(|u| {
            u.starts_with("magnet:?") || u.ends_with(".torrent") || u.starts_with("bt://")
        }) || metadata_info.is_some();

        let (bitfield, bt_info_hash, bt_saved_metadata_path) = if is_bt {
            let bf = bt_bitfield.unwrap_or_default();
            // Try to extract info hash from magnet URI
            let info_hash = raw_uris
                .iter()
                .find(|u| u.starts_with("magnet:?"))
                .and_then(|u| Self::extract_info_hash_from_magnet(u));
            let metadata_path = metadata_info
                .as_ref()
                .and_then(|info| info.metadata_path().map(str::to_owned));
            (bf, info_hash, metadata_path)
        } else {
            (vec![], None, None)
        };

        // Calculate resume offset for HTTP/FTP
        let resume_offset = if completed_length > 0 && !is_bt {
            Some(completed_length)
        } else {
            None
        };

        debug!(
            gid = %gid,
            protocol = if is_bt { "BT" } else { "HTTP/FTP" },
            completed = completed_length,
            total = total_length,
            "Extracted resume data from RequestGroup"
        );

        Ok(ResumeData {
            gid,
            uris,
            total_length,
            completed_length,
            uploaded_length,
            bitfield,
            num_pieces: None,   // Could be calculated from bitfield length
            piece_length: None, // Would need to be stored in RequestGroup
            status: status_str,
            error_message,
            last_download_time,
            created_at,
            output_path,
            checksum,
            options: options_map,
            resume_offset,
            bt_info_hash,
            bt_saved_metadata_path,
            #[cfg(feature = "metalink")]
            metalink_data: metalink_source
                .as_ref()
                .map(|(data, _)| base64::engine::general_purpose::STANDARD.encode(data)),
            #[cfg(not(feature = "metalink"))]
            metalink_data: None,
            #[cfg(feature = "metalink")]
            metalink_file_index: metalink_source.map(|(_, index)| index),
            #[cfg(not(feature = "metalink"))]
            metalink_file_index: None,
        })
    }

    fn to_restore_components(
        &self,
    ) -> (String, Vec<String>, HashMap<String, String>, RestoreState) {
        let gid = self.gid.clone();
        let uris: Vec<String> = self.uris.iter().map(|u| u.uri.clone()).collect();
        let options = self.options.clone();

        let restore_state = if self.is_bit_torrent() {
            RestoreState::BitTorrent {
                bitfield: self.bitfield.clone(),
                num_pieces: self.num_pieces,
                piece_length: self.piece_length,
                info_hash: self.bt_info_hash.clone(),
                metadata_path: self.bt_saved_metadata_path.clone(),
            }
        } else if self.is_metalink() && self.uris.len() > 1 {
            // Build mirror list with priority scoring
            let mirrors: Vec<MirrorRestoreInfo> = self
                .uris
                .iter()
                .enumerate()
                .map(|(i, u)| {
                    // Calculate priority: working mirrors first, then by speed
                    let mut priority = i as u32 * 10;
                    if u.tried && u.last_result.as_deref() == Some("ok") {
                        priority = 0; // Highest priority: working mirrors
                    } else if !u.tried {
                        priority += 5; // Untried mirrors get medium priority
                    } else if u.last_result.is_some() {
                        priority += 20; // Failed mirrors get lowest priority
                    }

                    MirrorRestoreInfo {
                        uri: u.uri.clone(),
                        tried: u.tried,
                        last_result: u.last_result.clone(),
                        speed_bytes_per_sec: u.speed_bytes_per_sec,
                        priority_score: priority,
                    }
                })
                .collect();

            // Sort by priority score (ascending = higher priority first)
            let mut sorted_mirrors = mirrors;
            sorted_mirrors.sort_by_key(|m| m.priority_score);

            RestoreState::Metalink {
                mirrors: sorted_mirrors,
                resume_offset: self.resume_offset,
            }
        } else {
            RestoreState::HttpFtp {
                resume_offset: self.resume_offset.unwrap_or(0),
                total_length: self.total_length,
                completed_length: self.completed_length,
            }
        };

        (gid, uris, options, restore_state)
    }
}
