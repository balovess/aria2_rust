//! Rich download result for RPC consumers.
//!
//! Mirrors C++ `DownloadResult` which carries the full download snapshot
//! for `aria2.tellStatus`, `aria2.tellStopped`, `aria2.getDownloadResult`
//! RPC methods. Contains GID, progress stats, file entries, BT info hash,
//! and relationship GIDs (followedBy / following / belongsTo).

use serde::{Deserialize, Serialize};

#[cfg(feature = "bittorrent")]
use crate::download::download_context::{ContextAttributeType, TorrentAttribute};

use super::GroupId;
use super::result_code::DownloadResultCode;
use super::status::DownloadStatus;
use crate::segment::piece_storage::BitfieldMan;

/// File entry within a download result.
///
/// Mirrors C++ `FileData` / `FileEntry` information exposed by RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File index (1-based, matching C++ convention).
    pub index: usize,
    /// File path relative to download directory.
    pub path: String,
    /// Total file length in bytes.
    pub length: u64,
    /// Completed bytes for this file.
    pub completed_length: u64,
    /// Whether this file is selected for download.
    pub selected: bool,
    /// URIs associated with this file.
    pub uris: Vec<UriEntry>,
}

/// URI entry within a file entry.
///
/// Mirrors C++ `URIResult` exposed by RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UriEntry {
    /// The URI string.
    pub uri: String,
    /// Current status of this URI ("used", "waiting", "spent").
    pub status: String,
}

/// BitTorrent metadata retained with a stopped result for RPC snapshots.
///
/// The live download context is released when a group is demoted. Keeping
/// this small owned projection preserves the original aria2 stopped-status
/// fields without retaining protocol state or sockets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BittorrentResultMetadata {
    pub announce_list: Vec<Vec<String>>,
    pub comment: Option<String>,
    pub creation_date: Option<i64>,
    pub mode: Option<String>,
    pub name: Option<String>,
}

#[cfg(feature = "bittorrent")]
impl BittorrentResultMetadata {
    pub(crate) fn from_torrent_attribute(attribute: &TorrentAttribute) -> Self {
        let mode = match attribute.mode {
            crate::download::download_context::BtFileMode::Single => "single",
            crate::download::download_context::BtFileMode::Multi => "multi",
        };
        Self {
            announce_list: attribute.announce_list.clone(),
            comment: (!attribute.comment.is_empty()).then(|| attribute.comment.clone()),
            creation_date: (attribute.creation_date != 0).then_some(attribute.creation_date),
            mode: Some(mode.to_string()),
            name: (!attribute.name.is_empty()).then(|| attribute.name.clone()),
        }
    }
}

/// Rich download result for RPC consumers.
///
/// Mirrors C++ `DownloadResult` with all fields needed for
/// `aria2.tellStatus`, `aria2.tellStopped`, and `aria2.getDownloadResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    // ── Identity ────────────────────────────────────────────────────────
    /// GID of the download this result refers to.
    pub gid: GroupId,
    /// Download status at the time of result creation.
    pub status: DownloadStatus,
    /// Structured result code.
    pub code: DownloadResultCode,
    /// Human-readable error / status message.
    pub message: String,
    /// The request-option snapshot owned by this terminal result.
    ///
    /// C++ aria2 retains the group's `Option` so `aria2.getOption` continues
    /// to work after the request group has moved into stopped storage. This
    /// is adapter-facing state, not part of the serialized result payload.
    #[serde(skip)]
    option_snapshot: Option<std::collections::HashMap<String, serde_json::Value>>,

    // ── Progress ────────────────────────────────────────────────────────
    /// Total length of the download in bytes.
    pub total_length: u64,
    /// Completed length in bytes.
    pub completed_length: u64,
    /// Total uploaded bytes (BT only, 0 for non-BT).
    pub upload_length: u64,
    /// Download speed in bytes/sec at the time of snapshot.
    pub download_speed: u64,
    /// Upload speed in bytes/sec at the time of snapshot (BT only).
    pub upload_speed: u64,
    /// Total number of pieces.
    pub num_pieces: u32,
    /// Piece length in bytes.
    pub piece_length: u32,
    /// Bitfield representing completed pieces (hex string for RPC).
    /// Empty string if not applicable.
    pub bitfield: String,

    // ── Relationships ──────────────────────────────────────────────────
    /// GIDs of downloads that were spawned by this one
    /// (e.g. Metalink → child downloads, torrent → magnet).
    pub followed_by: Vec<GroupId>,
    /// GID of the parent download that spawned this one.
    pub following: Option<GroupId>,
    /// GID of the download this one belongs to (e.g. BT parent).
    pub belongs_to: Option<GroupId>,

    // ── File info ──────────────────────────────────────────────────────
    /// Download directory.
    pub dir: String,
    /// File entries for multi-file downloads.
    pub files: Vec<FileEntry>,
    /// BT info hash (empty string for non-BT).
    pub info_hash: String,
    /// BT metadata needed after the live download context is released.
    pub bt_metadata: Option<BittorrentResultMetadata>,
    /// Raw torrent metadata retained only for session entries that may be
    /// written after this result has been detached from its RequestGroup.
    #[cfg(feature = "bittorrent")]
    #[serde(skip)]
    bt_metadata_data: Option<std::sync::Arc<Vec<u8>>>,

    // ── Metadata ───────────────────────────────────────────────────────
    /// Download context attributes (e.g. CTX_ATTR_ED2K for aria2-next).
    pub attrs: std::collections::HashMap<String, String>,
    /// Whether this was an in-memory download (metadata exchange only).
    pub in_memory_download: bool,
    /// Session download length (bytes downloaded since session start).
    pub session_download_length: u64,
    /// Session time (seconds since session start).
    pub session_time: u64,
}

impl DownloadResult {
    /// Create a new download result with identity fields and defaults.
    pub fn new(gid: GroupId, status: DownloadStatus, code: DownloadResultCode) -> Self {
        let message = match code {
            DownloadResultCode::Finished => "OK".to_string(),
            DownloadResultCode::Removed => "Download removed by user".to_string(),
            DownloadResultCode::InProgress => "Download interrupted by shutdown".to_string(),
            _ => format!("{}", code),
        };

        Self {
            gid,
            status,
            code,
            message,
            option_snapshot: None,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            num_pieces: 0,
            piece_length: 0,
            bitfield: String::new(),
            followed_by: Vec::new(),
            following: None,
            belongs_to: None,
            dir: String::new(),
            files: Vec::new(),
            info_hash: String::new(),
            bt_metadata: None,
            #[cfg(feature = "bittorrent")]
            bt_metadata_data: None,
            attrs: std::collections::HashMap::new(),
            in_memory_download: false,
            session_download_length: 0,
            session_time: 0,
        }
    }

    pub(crate) fn set_option_snapshot(
        &mut self,
        options: Option<std::collections::HashMap<String, serde_json::Value>>,
    ) {
        self.option_snapshot = options;
    }

    /// Return the option state captured when the request became terminal.
    pub fn option_snapshot(&self) -> Option<&std::collections::HashMap<String, serde_json::Value>> {
        self.option_snapshot.as_ref()
    }

    #[cfg(feature = "bittorrent")]
    pub(crate) fn bt_metadata_data(&self) -> Option<&[u8]> {
        self.bt_metadata_data.as_deref().map(Vec::as_slice)
    }

    /// Create a successful result (convenience for tests).
    pub fn finished() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Complete,
            code: DownloadResultCode::Finished,
            message: String::from("OK"),
            option_snapshot: None,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            num_pieces: 0,
            piece_length: 0,
            bitfield: String::new(),
            followed_by: Vec::new(),
            following: None,
            belongs_to: None,
            dir: String::new(),
            files: Vec::new(),
            info_hash: String::new(),
            bt_metadata: None,
            #[cfg(feature = "bittorrent")]
            bt_metadata_data: None,
            attrs: std::collections::HashMap::new(),
            in_memory_download: false,
            session_download_length: 0,
            session_time: 0,
        }
    }

    /// Create a result for a user-removed download.
    pub fn removed() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Removed,
            code: DownloadResultCode::Removed,
            message: String::from("Download removed by user"),
            ..Self::finished()
        }
    }

    /// Create a result for an interrupted (shutdown) download.
    pub fn in_progress() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Active,
            code: DownloadResultCode::InProgress,
            message: String::from("Download interrupted by shutdown"),
            ..Self::finished()
        }
    }

    /// Create a result for a paused download.
    pub fn paused() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Paused,
            // C++ has no paused error code. A paused task is not normally
            // stored as a stopped result; keep this constructor wire-safe for
            // callers that need a snapshot before requeueing.
            code: DownloadResultCode::UnknownError,
            message: String::from("Download paused"),
            ..Self::finished()
        }
    }

    /// Create an error result with a specific code and message.
    pub fn error(code: DownloadResultCode, message: impl Into<String>) -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Error(String::new()),
            code,
            message: message.into(),
            ..Self::finished()
        }
    }

    /// Get the GID as a hex string (for RPC compatibility).
    pub fn gid_hex(&self) -> String {
        self.gid.to_hex_string()
    }

    /// Fill in progress stats from the given `RequestGroup`.
    ///
    /// Reads `total_length`, `completed_length`, `upload_length`,
    /// `download_speed`, `upload_speed`, `dir`, and `info_hash`
    /// from the group's `AtomicProgress` and options.
    pub fn fill_from_group(&mut self, group: &super::RequestGroup) {
        self.total_length = group.total_length();
        self.completed_length = group.completed_length();
        self.upload_length = group.upload_length();
        self.download_speed = group.download_speed();
        self.upload_speed = group.upload_speed();
        self.session_time = group.elapsed_time().map_or(0, |elapsed| elapsed.as_secs());
        self.dir = group.options().dir.clone().unwrap_or_default();
        self.info_hash = group
            .info_hash_hex()
            .or_else(|| group.get_bt_info_hash_hex())
            .unwrap_or_default();
        self.in_memory_download = group.is_in_memory_download();

        #[cfg(feature = "bittorrent")]
        {
            self.bt_metadata = group.get_download_context().and_then(|context| {
                context
                    .get_attribute(ContextAttributeType::BitTorrent)
                    .and_then(|value| value.downcast_ref::<TorrentAttribute>())
                    .map(BittorrentResultMetadata::from_torrent_attribute)
            });

            let option_bool = |name: &str, default: bool| {
                self.option_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.get(name))
                    .and_then(crate::request::request_group::option_value_to_string)
                    .and_then(|value| value.parse::<bool>().ok())
                    .unwrap_or(default)
            };
            let resumable_result = match self.code {
                DownloadResultCode::Finished | DownloadResultCode::Removed => {
                    option_bool("force-save", false)
                }
                DownloadResultCode::ResourceNotFound | DownloadResultCode::MaxFileNotFound => {
                    option_bool("save-not-found", true)
                }
                _ => true,
            };
            if resumable_result {
                self.bt_metadata_data = group.bt_metadata_data().map(std::sync::Arc::new);
            }
        }

        let fallback_path = {
            if let Some(path) = group.resolved_output_path() {
                path
            } else {
                let options = group.options();
                let name = group
                    .output_name()
                    .or_else(|| options.out.clone())
                    .or_else(|| {
                        group
                            .uris()
                            .first()
                            .map(|uri| crate::validation::uri::sanitize_filename_from_uri(uri))
                    })
                    .unwrap_or_default();
                match options.dir.as_deref().filter(|dir| !dir.is_empty()) {
                    Some(dir) if !name.is_empty() => std::path::PathBuf::from(dir)
                        .join(name)
                        .to_string_lossy()
                        .into_owned(),
                    _ => name,
                }
            }
        };
        let resolved_path = group.resolved_output_path();
        let completion = bt_completion_bitfield(group);
        let files = if let Some(context) = group.get_download_context() {
            context
                .get_file_entries()
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    let path = if index == 0 {
                        resolved_path
                            .clone()
                            .unwrap_or_else(|| file.path().to_string())
                    } else {
                        file.path().to_string()
                    };
                    let completed_length = completion
                        .as_ref()
                        .map(|bitfield| {
                            bitfield.get_offset_completed_length(file.offset(), file.length())
                        })
                        .unwrap_or_else(|| {
                            self.completed_length
                                .saturating_sub(file.offset())
                                .min(file.length())
                        });
                    let uris = file
                        .uris()
                        .into_iter()
                        .map(|uri| {
                            let status = if file.remaining_uris().iter().any(|value| value == &uri)
                            {
                                "waiting"
                            } else if file.spent_uris().iter().any(|value| value == &uri) {
                                "used"
                            } else {
                                "spent"
                            };
                            UriEntry {
                                uri,
                                status: status.to_string(),
                            }
                        })
                        .collect();

                    FileEntry {
                        index: index + 1,
                        path,
                        length: file.length(),
                        completed_length,
                        selected: file.is_requested(),
                        uris,
                    }
                })
                .collect()
        } else {
            let uris = group
                .get_all_uris()
                .into_iter()
                .map(|uri| UriEntry {
                    uri,
                    status: "waiting".to_string(),
                })
                .collect();
            vec![FileEntry {
                index: 1,
                path: fallback_path.clone(),
                length: self.total_length,
                completed_length: self.completed_length,
                selected: true,
                uris,
            }]
        };
        self.files = if files.is_empty() {
            let uris = group
                .get_all_uris()
                .into_iter()
                .map(|uri| UriEntry {
                    uri,
                    status: "waiting".to_string(),
                })
                .collect();
            vec![FileEntry {
                index: 1,
                path: fallback_path,
                length: self.total_length,
                completed_length: self.completed_length,
                selected: true,
                uris,
            }]
        } else {
            files
        };

        self.num_pieces = group.get_bt_num_pieces();
        self.piece_length = group.get_bt_piece_length();
        if let Some(bitfield) = group.get_bt_bitfield() {
            self.bitfield = bitfield.iter().map(|byte| format!("{byte:02x}")).collect();
        }
    }
}

fn bt_completion_bitfield(group: &super::RequestGroup) -> Option<BitfieldMan> {
    let piece_length = group.get_bt_piece_length() as u64;
    let bitfield = group.get_bt_bitfield()?;
    if piece_length == 0 || bitfield.is_empty() {
        return None;
    }
    let mut completion = BitfieldMan::new(piece_length, group.get_total_length_atomic());
    completion.set_bitfield(&bitfield);
    Some(completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_from_group_derives_ddl_filename_when_no_output_is_configured() {
        let group = crate::request::request_group::RequestGroup::new(
            GroupId::new(1),
            vec!["https://example.com/releases/file.zip?download=1".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        );
        group.set_total_length(4096);

        let mut result = DownloadResult::finished();
        result.fill_from_group(&group);

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "file.zip");
        assert_eq!(result.files[0].length, 4096);
    }

    #[test]
    fn fill_from_group_uses_the_safe_decoded_url_segment() {
        let group = crate::request::request_group::RequestGroup::new(
            GroupId::new(2),
            vec!["https://example.com/releases/my%20file.zip?token=ignored#fragment".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        );
        group.set_total_length(4096);

        let mut result = DownloadResult::finished();
        result.fill_from_group(&group);

        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "my file.zip");
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn stopped_metadata_snapshot_matches_session_save_policy() {
        use crate::session::session_serializer::{
            serialize_groups_with_results, should_save_download_result,
        };

        let mut info = std::collections::BTreeMap::new();
        use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

        info.insert(b"length".to_vec(), BencodeValue::Int(1));
        info.insert(
            b"name".to_vec(),
            BencodeValue::Bytes(b"snapshot.bin".to_vec()),
        );
        info.insert(b"piece length".to_vec(), BencodeValue::Int(16));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20]));
        let mut torrent = std::collections::BTreeMap::new();
        torrent.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
        );
        torrent.insert(b"info".to_vec(), BencodeValue::Dict(info));
        let metadata = BencodeValue::Dict(torrent).encode();
        let info_hash = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&metadata)
            .expect("fixture torrent metadata parses")
            .info_hash
            .as_hex();
        let group = crate::request::request_group::RequestGroup::new(
            GroupId::new(3),
            vec![format!("bt://{info_hash}")],
            crate::request::request_group::DownloadOptions::default(),
        );
        group.set_bt_metadata_data(metadata.to_vec());

        let cases = [
            (
                DownloadResultCode::Finished,
                Some(("force-save", false)),
                false,
            ),
            (
                DownloadResultCode::Finished,
                Some(("force-save", true)),
                true,
            ),
            (
                DownloadResultCode::Removed,
                Some(("force-save", false)),
                false,
            ),
            (
                DownloadResultCode::Removed,
                Some(("force-save", true)),
                true,
            ),
            (DownloadResultCode::InProgress, None, true),
            (
                DownloadResultCode::ResourceNotFound,
                Some(("save-not-found", false)),
                false,
            ),
            (
                DownloadResultCode::ResourceNotFound,
                Some(("save-not-found", true)),
                true,
            ),
            (
                DownloadResultCode::MaxFileNotFound,
                Some(("save-not-found", false)),
                false,
            ),
            (
                DownloadResultCode::MaxFileNotFound,
                Some(("save-not-found", true)),
                true,
            ),
            (DownloadResultCode::TimeOut, None, true),
        ];

        for (index, (code, option, expected_save)) in cases.into_iter().enumerate() {
            let snapshot = option
                .map(|(key, value)| {
                    std::collections::HashMap::from([(
                        key.to_string(),
                        serde_json::Value::Bool(value),
                    )])
                })
                .unwrap_or_default();
            let mut result = DownloadResult::new(
                GroupId::new(10 + index as u64),
                DownloadStatus::Complete,
                code,
            );
            result.set_option_snapshot(Some(snapshot));
            result.fill_from_group(&group);

            assert_eq!(
                result.bt_metadata_data().is_some(),
                expected_save,
                "metadata retention for {code:?} with option {option:?}"
            );
            assert_eq!(
                should_save_download_result(&result),
                expected_save,
                "session-save policy for {code:?} with option {option:?}"
            );
            let serialized =
                serialize_groups_with_results(&[], &[result]).expect("session results serialize");
            assert_eq!(
                serialized.contains("aria2-rust-bt-metadata-data="),
                expected_save,
                "serialized metadata for {code:?} with option {option:?}"
            );
            if code == DownloadResultCode::Finished && expected_save {
                use base64::Engine as _;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(
                        serialized
                            .split("aria2-rust-bt-metadata-data=")
                            .nth(1)
                            .expect("serialized torrent metadata option")
                            .split_whitespace()
                            .next()
                            .expect("base64 metadata value"),
                    )
                    .expect("serialized metadata is base64");
                let decoded: Vec<u8> =
                    serde_json::from_slice(&decoded).expect("metadata descriptor is JSON bytes");
                assert_eq!(decoded, metadata);
            }
        }
    }

    #[test]
    fn test_result_code_roundtrip() {
        for code in [
            DownloadResultCode::Finished,
            DownloadResultCode::TimeOut,
            DownloadResultCode::Removed,
            DownloadResultCode::InProgress,
            DownloadResultCode::ChecksumError,
        ] {
            assert_eq!(DownloadResultCode::from_code(code.as_code()), Some(code));
        }
    }

    #[test]
    fn test_is_success() {
        assert!(DownloadResultCode::Finished.is_success());
        assert!(!DownloadResultCode::TimeOut.is_success());
    }

    #[test]
    fn test_is_resumable() {
        assert!(DownloadResultCode::InProgress.is_resumable());
        assert!(!DownloadResultCode::TimeOut.is_resumable());
    }

    #[test]
    fn test_is_user_stopped() {
        assert!(DownloadResultCode::Removed.is_user_stopped());
        assert!(!DownloadResultCode::InProgress.is_user_stopped());
    }

    #[test]
    fn test_download_result_finished() {
        let r = DownloadResult::finished();
        assert_eq!(r.code, DownloadResultCode::Finished);
        assert_eq!(r.total_length, 0);
        assert!(r.followed_by.is_empty());
    }

    #[test]
    fn test_download_result_removed() {
        let r = DownloadResult::removed();
        assert_eq!(r.code, DownloadResultCode::Removed);
    }

    #[test]
    fn test_download_result_in_progress() {
        let r = DownloadResult::in_progress();
        assert_eq!(r.code, DownloadResultCode::InProgress);
    }

    #[test]
    fn test_download_result_paused_has_no_rust_only_error_code() {
        let r = DownloadResult::paused();
        assert_eq!(r.status, DownloadStatus::Paused);
        assert_eq!(r.code, DownloadResultCode::UnknownError);
        assert_ne!(r.code.as_code(), 33);
    }

    #[test]
    fn test_download_result_has_all_rpc_fields() {
        let r = DownloadResult::finished();
        // Verify all fields that RPC consumers expect are present.
        assert_eq!(r.gid_hex(), "0000000000000000");
        assert_eq!(r.total_length, 0);
        assert_eq!(r.completed_length, 0);
        assert_eq!(r.upload_length, 0);
        assert_eq!(r.download_speed, 0);
        assert_eq!(r.upload_speed, 0);
        assert_eq!(r.num_pieces, 0);
        assert_eq!(r.piece_length, 0);
        assert!(r.bitfield.is_empty());
        assert!(r.followed_by.is_empty());
        assert!(r.following.is_none());
        assert!(r.belongs_to.is_none());
        assert!(r.dir.is_empty());
        assert!(r.files.is_empty());
        assert!(r.info_hash.is_empty());
        assert!(r.attrs.is_empty());
        assert!(!r.in_memory_download);
        assert_eq!(r.session_download_length, 0);
        assert_eq!(r.session_time, 0);
    }
}
