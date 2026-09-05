//! UI module for aria2-rust CLI
//!
//! Contains terminal-based display components including
//! progress bars, status indicators, and TUI elements.

pub mod console_progress;
pub mod progress_bar;
#[cfg(feature = "tui")]
pub mod resources;

#[cfg(feature = "tui")]
pub mod tui;

#[cfg(not(feature = "tui"))]
pub mod tui {
    use aria2_core::engine::engine_command::EngineCommandSender;
    use aria2_core::request::request_group::DownloadOptions;
    use aria2_core::request::request_group_man::RequestGroupMan;

    pub async fn run(
        _request_man: std::sync::Arc<RequestGroupMan>,
        _command_tx: EngineCommandSender,
        _language: Option<String>,
        _options: Option<DownloadOptions>,
    ) -> Result<(), String> {
        Err("TUI support is not enabled; rebuild with `--features tui`".to_string())
    }

    pub async fn run_remote(
        _url: String,
        _secret: Option<String>,
        _language: Option<String>,
    ) -> Result<(), String> {
        Err("TUI support is not enabled; rebuild with `--features tui`".to_string())
    }
}
