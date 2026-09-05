//! Startup mode resolution for the application runtime.
//!
//! This module owns the distinction between a one-shot download process and
//! a long-lived RPC process. Daemonization is deliberately not part of this
//! decision: it changes how the process is detached, not what keeps the
//! engine alive.

/// The application-level execution modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunMode {
    /// Run the supplied downloads and exit when they finish.
    OneShotDownload,
    /// Wait for RPC requests without initial downloads.
    RpcService,
    /// Run initial downloads while continuing to accept RPC requests.
    DownloadWithRpc,
    /// Run initial downloads under the local interactive terminal UI.
    Tui,
}

/// Facts collected by the configuration/input phase before the engine starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StartupInputs {
    pub(super) has_initial_downloads: bool,
    pub(super) has_input_file: bool,
    pub(super) restored_tasks: usize,
    pub(super) tui: bool,
    pub(super) configured_rpc: bool,
    pub(super) explicit_rpc: Option<bool>,
}

/// The resolved contract shared by the engine and RPC startup paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StartupPlan {
    mode: RunMode,
}

impl StartupPlan {
    /// Resolve the process contract from startup inputs and final config.
    ///
    /// A configured RPC server is inherited by a command only when that
    /// command did not request downloads. An explicit CLI `--enable-rpc=true`
    /// opts into the combined mode; an explicit `false` always opts out.
    pub(super) fn resolve(inputs: StartupInputs) -> Result<Self, String> {
        let has_download_request =
            inputs.has_initial_downloads || inputs.has_input_file || inputs.restored_tasks > 0;
        let has_download_work = inputs.has_initial_downloads || inputs.restored_tasks > 0;
        let rpc_requested = inputs
            .explicit_rpc
            .unwrap_or(inputs.configured_rpc && !has_download_request);

        let mode = if inputs.tui {
            RunMode::Tui
        } else if has_download_work {
            if rpc_requested {
                RunMode::DownloadWithRpc
            } else {
                RunMode::OneShotDownload
            }
        } else if rpc_requested {
            RunMode::RpcService
        } else {
            return Err(
                "Please provide a download URI, torrent/metalink file, or input-file session, or enable RPC"
                    .to_string(),
            );
        };

        Ok(Self { mode })
    }

    pub(super) const fn starts_rpc(self) -> bool {
        matches!(self.mode, RunMode::RpcService | RunMode::DownloadWithRpc)
    }

    pub(super) const fn keeps_engine_alive(self) -> bool {
        self.starts_rpc() || matches!(self.mode, RunMode::Tui)
    }

    pub(super) const fn is_rpc_service(self) -> bool {
        matches!(self.mode, RunMode::RpcService)
    }

    #[cfg(test)]
    pub(super) const fn mode(self) -> RunMode {
        self.mode
    }
}

#[cfg(test)]
mod tests {
    use super::{RunMode, StartupInputs, StartupPlan};

    #[test]
    fn config_rpc_is_ignored_for_one_shot_downloads() {
        let plan = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: true,
            has_input_file: false,
            restored_tasks: 0,
            tui: false,
            configured_rpc: true,
            explicit_rpc: None,
        })
        .unwrap();
        assert_eq!(plan.mode(), RunMode::OneShotDownload);
        assert!(!plan.starts_rpc());
        assert!(!plan.keeps_engine_alive());
    }

    #[test]
    fn explicit_rpc_enables_combined_mode() {
        let plan = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: true,
            has_input_file: false,
            restored_tasks: 0,
            tui: false,
            configured_rpc: false,
            explicit_rpc: Some(true),
        })
        .unwrap();
        assert_eq!(plan.mode(), RunMode::DownloadWithRpc);
        assert!(plan.starts_rpc());
        assert!(plan.keeps_engine_alive());
    }

    #[test]
    fn rpc_only_mode_uses_configured_rpc() {
        let plan = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: false,
            has_input_file: false,
            restored_tasks: 0,
            tui: false,
            configured_rpc: true,
            explicit_rpc: None,
        })
        .unwrap();
        assert_eq!(plan.mode(), RunMode::RpcService);
        assert!(plan.starts_rpc());
    }

    #[test]
    fn session_input_suppresses_config_only_rpc() {
        let plan = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: false,
            has_input_file: true,
            restored_tasks: 1,
            tui: false,
            configured_rpc: true,
            explicit_rpc: None,
        })
        .unwrap();
        assert_eq!(plan.mode(), RunMode::OneShotDownload);
        assert!(!plan.starts_rpc());
    }

    #[test]
    fn explicit_false_disables_rpc() {
        let result = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: false,
            has_input_file: false,
            restored_tasks: 0,
            tui: false,
            configured_rpc: true,
            explicit_rpc: Some(false),
        });
        assert!(result.is_err());
    }

    #[test]
    fn tui_mode_keeps_engine_alive_without_starting_rpc() {
        let plan = StartupPlan::resolve(StartupInputs {
            has_initial_downloads: false,
            has_input_file: false,
            restored_tasks: 0,
            tui: true,
            configured_rpc: false,
            explicit_rpc: None,
        })
        .unwrap();
        assert_eq!(plan.mode(), RunMode::Tui);
        assert!(!plan.starts_rpc());
        assert!(plan.keeps_engine_alive());
    }
}
