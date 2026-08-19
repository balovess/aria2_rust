//! In-process RPC fixture backed by the real aria2 core adapter.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use aria2::app::rpc_backend::CoreRpcBackend;
use aria2_core::config::ConfigManager;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use tokio::sync::RwLock;

static RETAINED_COMMAND_RECEIVERS: OnceLock<
    Mutex<Vec<tokio::sync::mpsc::UnboundedReceiver<EngineCommand>>>,
> = OnceLock::new();

/// An RPC engine plus the core state it adapts. The receiver is retained so
/// backend lifecycle commands remain deliverable even when no download loop is
/// running in a focused RPC test.
pub struct RpcFixture {
    pub engine: RpcEngine,
    pub group_man: Arc<RequestGroupMan>,
    _command_receiver: tokio::sync::mpsc::UnboundedReceiver<EngineCommand>,
}

impl RpcFixture {
    pub fn new(save_session_path: Option<PathBuf>) -> Self {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, command_receiver) =
            tokio::sync::mpsc::unbounded_channel::<EngineCommand>();
        let config = Arc::new(RwLock::new(ConfigManager::new_with_identity(
            aria2::identity::DEFAULT_USER_AGENT,
            aria2::identity::DEFAULT_PEER_AGENT,
        )));
        let backend = CoreRpcBackend::new(
            Arc::clone(&group_man),
            command_sender.into(),
            config,
            save_session_path,
            aria2::identity::PRODUCT_VERSION,
        );

        Self {
            engine: RpcEngine::with_backend(Arc::new(backend)),
            group_man,
            _command_receiver: command_receiver,
        }
    }

    pub fn into_engine(self) -> RpcEngine {
        let Self {
            engine,
            _command_receiver,
            ..
        } = self;
        RETAINED_COMMAND_RECEIVERS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(_command_receiver);
        engine
    }
}

pub fn engine() -> RpcEngine {
    RpcFixture::new(None).into_engine()
}

pub fn engine_with_save_session_path(path: PathBuf) -> RpcEngine {
    RpcFixture::new(Some(path)).into_engine()
}
