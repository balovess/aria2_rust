//! Parameter parsing and wire shaping for status RPC methods.

use std::collections::HashSet;

use serde::Serialize;

use crate::backend::{BackendRequest, BackendResponse};
use crate::json_rpc::{JsonRpcError, JsonRpcRequest};
use crate::types::StatusInfo;

pub(crate) struct StatusKeyFilter {
    keys: HashSet<String>,
}

pub(crate) fn status_key_filter(keys: &[String]) -> Option<StatusKeyFilter> {
    (!keys.is_empty()).then(|| StatusKeyFilter {
        keys: keys.iter().cloned().collect(),
    })
}

pub(crate) fn parse_tell_active(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    Ok(BackendRequest::TellActive {
        keys: req
            .take_optional_param::<Vec<String>>(0)?
            .unwrap_or_default(),
    })
}

fn parse_pagination(req: &mut JsonRpcRequest) -> Result<(i64, usize, Vec<String>), JsonRpcError> {
    let offset = req.take_param(0)?;
    let num = req.take_param::<i64>(1)?;
    if num < 0 {
        return Err(JsonRpcError::RpcExecution(
            "num must be greater than or equal to 0".into(),
        ));
    }
    let num = usize::try_from(num)
        .map_err(|_| JsonRpcError::RpcExecution("num is out of range".into()))?;
    let keys = req
        .take_optional_param::<Vec<String>>(2)?
        .unwrap_or_default();
    Ok((offset, num, keys))
}

pub(crate) fn parse_tell_waiting(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let (offset, num, keys) = parse_pagination(req)?;
    Ok(BackendRequest::TellWaiting { offset, num, keys })
}

pub(crate) fn parse_tell_stopped(req: &mut JsonRpcRequest) -> Result<BackendRequest, JsonRpcError> {
    let (offset, num, keys) = parse_pagination(req)?;
    Ok(BackendRequest::TellStopped { offset, num, keys })
}

/// Apply the optional `keys` projection after the backend has built a
/// semantically complete DTO. This keeps the projection policy in the wire
/// layer and lets every backend use the same public shape.
pub(crate) fn serialize_status_response(
    response: BackendResponse,
    keys: &[String],
) -> Result<serde_json::Value, JsonRpcError> {
    let filter = status_key_filter(keys);
    match response {
        BackendResponse::Status(status) => serialize_status(status, filter.as_ref()),
        BackendResponse::Statuses(statuses) => statuses
            .into_iter()
            .map(|status| serialize_status(status, filter.as_ref()))
            .collect(),
        other => other.into_json_value().map_err(super::backend_error),
    }
}

fn serialize_status(
    status: StatusInfo,
    filter: Option<&StatusKeyFilter>,
) -> Result<serde_json::Value, JsonRpcError> {
    let Some(filter) = filter else {
        return serde_json::to_value(status).map_err(|error| {
            JsonRpcError::InternalError(format!("Serialization failed: {error}"))
        });
    };

    let mut fields = serde_json::Map::with_capacity(filter.keys.len());

    macro_rules! add {
        ($key:literal, $value:expr) => {
            if filter.keys.contains($key) {
                fields.insert(
                    $key.to_string(),
                    serde_json::to_value($value).map_err(|error| {
                        JsonRpcError::InternalError(format!("Serialization failed: {error}"))
                    })?,
                );
            }
        };
    }

    macro_rules! add_optional {
        ($key:literal, $value:expr) => {
            if filter.keys.contains($key) {
                insert_optional(&mut fields, $key, $value)?;
            }
        };
    }

    macro_rules! add_number {
        ($key:literal, $value:expr) => {
            if filter.keys.contains($key) {
                insert_optional_display(&mut fields, $key, $value)?;
            }
        };
    }

    add!("gid", &status.gid);
    add_number!("totalLength", &status.total_length);
    add_number!("completedLength", &status.completed_length);
    add_number!("uploadLength", &status.upload_length);
    add_number!("downloadSpeed", &status.download_speed);
    add_number!("uploadSpeed", &status.upload_speed);
    add_number!("connections", &status.connections);
    add_number!("errorCode", &status.error_code);
    add_optional!("errorMessage", &status.error_message);
    add!("status", status.status.as_str());
    add_optional!("dir", &status.dir);
    add_optional!("files", &status.files);
    add_optional!("bittorrent", &status.bittorrent);
    add_optional!("following", &status.following);
    add_optional!("seeder", &status.seeder);
    add_optional!("bitfield", &status.bitfield);
    add_number!("pieceLength", &status.piece_length);
    add_number!("numPieces", &status.num_pieces);
    add_number!("completedPieces", &status.completed_pieces);
    add_number!("missingPieces", &status.missing_pieces);
    add_optional!("followedBy", &status.followed_by);
    add_optional!("belongsTo", &status.belongs_to);
    add_optional!("infoHash", &status.info_hash);
    add_number!("numSeeders", &status.num_seeders);
    add_number!("verifiedLength", &status.verified_length);
    add_optional!("verifyIntegrityPending", &status.verify_integrity_pending);

    Ok(serde_json::Value::Object(fields))
}

fn insert_optional<T: Serialize>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &Option<T>,
) -> Result<(), JsonRpcError> {
    if let Some(value) = value {
        fields.insert(
            key.to_string(),
            serde_json::to_value(value).map_err(|error| {
                JsonRpcError::InternalError(format!("Serialization failed: {error}"))
            })?,
        );
    }
    Ok(())
}

fn insert_optional_display<T: std::fmt::Display>(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: &Option<T>,
) -> Result<(), JsonRpcError> {
    if let Some(value) = value {
        fields.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::serialize_status_response;
    use crate::backend::BackendResponse;
    use crate::types::{DownloadStatus, StatusInfo};

    #[test]
    fn selective_status_serialization_keeps_wire_types_and_requested_keys() {
        let status = StatusInfo::new("gid-1")
            .with_total_length(1024)
            .with_completed_length(512)
            .with_status(DownloadStatus::Active)
            .with_dir("/tmp/downloads");

        let value = serialize_status_response(
            BackendResponse::Status(status),
            &["gid".into(), "completedLength".into(), "status".into()],
        )
        .unwrap();

        assert_eq!(value["gid"], "gid-1");
        assert_eq!(value["completedLength"], "512");
        assert_eq!(value["status"], "active");
        assert!(value.get("totalLength").is_none());
        assert!(value.get("dir").is_none());
    }

    #[test]
    fn unfiltered_status_serialization_keeps_complete_shape() {
        let status = StatusInfo::new("gid-2")
            .with_total_length(2048)
            .with_completed_length(1024)
            .with_status(DownloadStatus::Paused);

        let value = serialize_status_response(BackendResponse::Status(status), &[]).unwrap();

        assert_eq!(value["gid"], "gid-2");
        assert_eq!(value["totalLength"], "2048");
        assert_eq!(value["completedLength"], "1024");
        assert_eq!(value["status"], "paused");
    }
}
