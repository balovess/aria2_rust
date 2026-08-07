use std::sync::Arc;

use super::DownloadEngine;
use crate::error::{Aria2Error, Result};

impl DownloadEngine {
    /// Run the engine loop using typed `EngineCommand` messages and
    /// `RequestGroupMan` promotion/demotion.
    pub async fn run(mut self) -> Result<()> {
        let group_man = self.request_group_man.take().ok_or_else(|| {
            Aria2Error::DownloadFailed("run requires request_group_man to be set".to_string())
        })?;

        let shutdown_rx = self
            .shutdown_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed("shutdown_rx already taken".to_string()))?;

        let engine_cmd_rx = self
            .engine_cmd_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed("engine_cmd_rx already taken".to_string()))?;

        let ctx = super::super::engine_loop::EngineLoopContext {
            group_man,
            ftp_pool: Arc::clone(&self.ftp_pool),
            dns_cache: Arc::clone(&self.dns_cache),
            auto_save: self.auto_save.take(),
            // Share the engine's bus so listeners registered before the loop are reached.
            event_hooks: Arc::clone(&self.event_hooks),
            // Use the process-wide file allocation manager owned by the engine layer.
            file_alloc_man: super::super::super::filesystem::file_allocation_man::shared(),
            keep_alive: self.keep_alive,
            server_stat_man: super::super::super::selector::server_stat_man::ServerStatMan::shared(
            )
            .clone(),
            // Keep a shared limiter even when no limit was configured. This
            // gives runtime RPC changes a stable handle that is already
            // present in every spawned command.
            global_limiter: Some(
                self.global_limiter
                    .take()
                    .unwrap_or_else(crate::rate_limiter::RateLimiter::unlimited),
            ),
        };

        super::super::engine_loop::run_engine_loop(
            ctx,
            engine_cmd_rx,
            shutdown_rx,
            self.tick_interval,
        )
        .await;

        Ok(())
    }

    pub async fn shutdown_engine(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}
