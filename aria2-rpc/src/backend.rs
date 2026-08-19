//! The typed boundary between the RPC transport and a download engine.
//!
//! `aria2-rpc` deliberately knows nothing about request-group storage,
//! engine command channels, or protocol implementations.  Applications wire
//! those details through [`RpcBackend`].  Keeping the boundary typed makes a
//! new backend (a real engine, a test double, or a remote proxy) possible
//! without teaching the wire layer about its internals.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::types::{FileInfo, GlobalStat, PeerInfo, ServerInfoIndex, StatusInfo, UriEntry};

/// Metadata advertised by a backend through `system.listMethods` and
/// `aria2.getVersion`.
#[derive(Debug, Clone)]
pub struct BackendMetadata {
    pub product_version: String,
    pub enabled_features: Vec<String>,
    pub methods: Vec<String>,
    pub notifications: Vec<String>,
}

impl BackendMetadata {
    /// Build metadata for the protocol-independent baseline.
    pub fn base(product_version: impl Into<String>) -> Self {
        Self {
            product_version: product_version.into(),
            enabled_features: vec![
                "Async DNS".to_string(),
                "Firefox3 Cookie".to_string(),
                "GZip".to_string(),
                "HTTPS".to_string(),
                "Message Digest".to_string(),
                "XML-RPC".to_string(),
            ],
            methods: base_method_names(),
            notifications: base_notification_names(),
        }
    }

    /// Add BitTorrent capabilities while preserving aria2's catalog order.
    pub fn with_bittorrent(mut self) -> Self {
        self.enabled_features.insert(1, "BitTorrent".to_string());
        self.methods.splice(
            1..1,
            ["aria2.addTorrent", "aria2.getPeers"].map(str::to_string),
        );
        self.notifications
            .push("aria2.onBtDownloadComplete".to_string());
        self
    }

    /// Add Metalink capability while preserving aria2's catalog order.
    pub fn with_metalink(mut self) -> Self {
        self.enabled_features.insert(5, "Metalink".to_string());
        let insert_at = self
            .methods
            .iter()
            .position(|method| method == "aria2.remove")
            .unwrap_or(1);
        self.methods
            .insert(insert_at, "aria2.addMetalink".to_string());
        self
    }

    /// Add SFTP capability to `aria2.getVersion`.
    pub fn with_sftp(mut self) -> Self {
        self.enabled_features.push("SFTP".to_string());
        self
    }
}

fn base_method_names() -> Vec<String> {
    [
        "aria2.addUri",
        "aria2.remove",
        "aria2.pause",
        "aria2.forcePause",
        "aria2.pauseAll",
        "aria2.forcePauseAll",
        "aria2.unpause",
        "aria2.unpauseAll",
        "aria2.forceRemove",
        "aria2.changePosition",
        "aria2.tellStatus",
        "aria2.getUris",
        "aria2.getFiles",
        "aria2.getServers",
        "aria2.tellActive",
        "aria2.tellWaiting",
        "aria2.tellStopped",
        "aria2.getOption",
        "aria2.changeUri",
        "aria2.changeOption",
        "aria2.getGlobalOption",
        "aria2.changeGlobalOption",
        "aria2.purgeDownloadResult",
        "aria2.removeDownloadResult",
        "aria2.getVersion",
        "aria2.getSessionInfo",
        "aria2.shutdown",
        "aria2.forceShutdown",
        "aria2.getGlobalStat",
        "aria2.saveSession",
        "system.multicall",
        "system.listMethods",
        "system.listNotifications",
    ]
    .map(str::to_string)
    .to_vec()
}

fn base_notification_names() -> Vec<String> {
    [
        "aria2.onDownloadStart",
        "aria2.onDownloadPause",
        "aria2.onDownloadStop",
        "aria2.onDownloadComplete",
        "aria2.onDownloadError",
    ]
    .map(str::to_string)
    .to_vec()
}

/// Queue operation used by `aria2.changePosition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionMode {
    SetFromStart,
    MoveFromStart,
    SetFromEnd,
}

/// One operation requested from a backend.
#[derive(Debug, Clone)]
pub enum BackendRequest {
    AddUri {
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    },
    AddTorrent {
        data: Vec<u8>,
        additional_uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    },
    AddMetalink {
        data: Vec<u8>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    },
    Remove {
        gid: String,
    },
    Pause {
        gid: String,
    },
    ForcePause {
        gid: String,
    },
    Unpause {
        gid: String,
    },
    TellStatus {
        gid: String,
        keys: Vec<String>,
    },
    TellActive {
        keys: Vec<String>,
    },
    TellWaiting {
        offset: i64,
        num: usize,
        keys: Vec<String>,
    },
    TellStopped {
        offset: i64,
        num: usize,
        keys: Vec<String>,
    },
    GetGlobalStat,
    GetUris {
        gid: String,
    },
    GetFiles {
        gid: String,
    },
    GetServers {
        gid: String,
    },
    PurgeDownloadResult,
    RemoveDownloadResult {
        gid: String,
    },
    GetGlobalOption,
    ChangeGlobalOption {
        options: HashMap<String, serde_json::Value>,
    },
    GetOption {
        gid: String,
    },
    ChangeOption {
        gid: String,
        options: HashMap<String, serde_json::Value>,
    },
    GetPeers {
        gid: String,
    },
    PauseAll,
    ForcePauseAll,
    UnpauseAll,
    ChangeUri {
        gid: String,
        file_index: usize,
        delete_uris: Vec<String>,
        add_uris: Vec<String>,
        position: Option<usize>,
    },
    SaveSession,
    ChangePosition {
        gid: String,
        position: i32,
        mode: PositionMode,
    },
    ForceRemove {
        gids: Vec<String>,
    },
    Shutdown {
        force: bool,
    },
}

/// A consistent read view used by a polling batch.
#[derive(Debug, Clone)]
pub struct BackendReadSnapshot {
    pub active: Vec<StatusInfo>,
    pub waiting: Vec<StatusInfo>,
    pub stopped: Vec<StatusInfo>,
    pub global_stat: GlobalStat,
}

/// Lifecycle effects that the transport should publish after a successful
/// backend operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    DownloadStart(String),
    DownloadPause(String),
    DownloadStop(String),
}

/// Result of a backend operation, including notifications caused by it.
#[derive(Debug, Clone)]
pub struct BackendResult {
    pub response: BackendResponse,
    pub events: Vec<BackendEvent>,
}

impl BackendResult {
    pub fn response(response: BackendResponse) -> Self {
        Self {
            response,
            events: Vec::new(),
        }
    }

    pub fn with_events(response: BackendResponse, events: Vec<BackendEvent>) -> Self {
        Self { response, events }
    }
}

/// Typed result variants that the RPC wire layer knows how to serialize.
// Keep the common status response inline; boxing it would add an allocation to
// every tellStatus call solely to reduce enum size.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum BackendResponse {
    Ok,
    Text(String),
    Gid(String),
    Gids(Vec<String>),
    Status(StatusInfo),
    Statuses(Vec<StatusInfo>),
    GlobalStat(GlobalStat),
    Uris(Vec<UriEntry>),
    Files(Vec<FileInfo>),
    Servers(Vec<ServerInfoIndex>),
    Peers(Vec<PeerInfo>),
    Options(HashMap<String, serde_json::Value>),
    Position(usize),
    Counts([usize; 2]),
}

impl BackendResponse {
    pub fn into_json_value(self) -> Result<serde_json::Value, BackendError> {
        match self {
            Self::Ok => Ok(serde_json::json!("OK")),
            Self::Text(text) => Ok(serde_json::Value::String(text)),
            Self::Gid(gid) => Ok(serde_json::json!(gid)),
            Self::Gids(gids) => serde_json::to_value(gids),
            Self::Status(status) => serde_json::to_value(status),
            Self::Statuses(statuses) => serde_json::to_value(statuses),
            Self::GlobalStat(stat) => serde_json::to_value(stat),
            Self::Uris(uris) => serde_json::to_value(uris),
            Self::Files(files) => serde_json::to_value(files),
            Self::Servers(servers) => serde_json::to_value(servers),
            Self::Peers(peers) => serde_json::to_value(peers),
            Self::Options(options) => serde_json::to_value(options),
            Self::Position(position) => serde_json::to_value(position),
            Self::Counts(counts) => serde_json::to_value(counts.map(|count| count.to_string())),
        }
        .map_err(|error| BackendError::Internal(format!("Serialization failed: {error}")))
    }
}

/// Errors at the domain boundary. Parameter syntax errors are produced by the
/// RPC parser; semantic validation belongs to the backend that owns the
/// option/task model.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    InvalidParams(String),
    #[error("{0}")]
    Execution(String),
    #[error("{0}")]
    Internal(String),
    #[error("unsupported backend operation: {0}")]
    Unsupported(String),
}

/// Application-owned implementation of the download-management side of RPC.
#[async_trait]
pub trait RpcBackend: Send + Sync {
    fn metadata(&self) -> BackendMetadata;

    /// Return the number of live, non-terminal tasks owned by the backend.
    async fn task_count(&self) -> usize {
        0
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError>;

    /// Capture one read view for a concurrent polling batch. Backends that do
    /// not need a snapshot can keep the default implementation.
    async fn capture_read_snapshot(
        &self,
    ) -> Result<Option<Arc<BackendReadSnapshot>>, BackendError> {
        Ok(None)
    }

    /// Execute a request against a previously captured read view.
    async fn execute_with_snapshot(
        &self,
        request: BackendRequest,
        snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> Result<BackendResult, BackendError> {
        let _ = snapshot;
        self.execute(request).await
    }
}

/// A pure-RPC fallback used by library/server construction tests. It makes
/// the absence of an application backend explicit and never creates core
/// state behind the caller's back.
#[derive(Debug, Default)]
pub struct UnsupportedBackend;

#[async_trait]
impl RpcBackend for UnsupportedBackend {
    fn metadata(&self) -> BackendMetadata {
        BackendMetadata::base(env!("CARGO_PKG_VERSION"))
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        Err(BackendError::Unsupported(format!("{request:?}")))
    }
}
