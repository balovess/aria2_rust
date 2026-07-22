use serde::{Deserialize, Serialize};
use std::fmt;

/// Status of a download task.
///
/// Custom serde impl serializes every variant as a plain lowercase string
/// (e.g. "active", "waiting", "error"), matching C++ aria2's wire format that
/// browser plugins expect. The `Error(String)` payload (error message) is not
/// emitted here; callers surface it via `StatusInfo.error_message` instead.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DownloadStatus {
    #[default]
    Waiting,
    Active,
    Paused,
    Error(String),
    Complete,
    Removed,
}

impl Serialize for DownloadStatus {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DownloadStatus {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "waiting" => Ok(DownloadStatus::Waiting),
            "active" => Ok(DownloadStatus::Active),
            "paused" => Ok(DownloadStatus::Paused),
            "error" => Ok(DownloadStatus::Error(String::new())),
            "complete" => Ok(DownloadStatus::Complete),
            "removed" => Ok(DownloadStatus::Removed),
            _ => Err(serde::de::Error::unknown_variant(
                "DownloadStatus",
                &[
                    "waiting", "active", "paused", "error", "complete", "removed",
                ],
            )),
        }
    }
}

impl fmt::Display for DownloadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DownloadStatus::Waiting => write!(f, "waiting"),
            DownloadStatus::Active => write!(f, "active"),
            DownloadStatus::Paused => write!(f, "paused"),
            DownloadStatus::Error(_) => write!(f, "error"),
            DownloadStatus::Complete => write!(f, "complete"),
            DownloadStatus::Removed => write!(f, "removed"),
        }
    }
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, DownloadStatus::Active | DownloadStatus::Waiting)
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, DownloadStatus::Complete)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, DownloadStatus::Paused)
    }

    pub fn is_stopped(&self) -> bool {
        !self.is_active() && !matches!(self, DownloadStatus::Removed)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadStatus::Active => "active",
            DownloadStatus::Waiting => "waiting",
            DownloadStatus::Paused => "paused",
            DownloadStatus::Error(_) => "error",
            DownloadStatus::Complete => "complete",
            DownloadStatus::Removed => "removed",
        }
    }
}
