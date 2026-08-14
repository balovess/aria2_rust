use tracing::info;

use super::BtDownloadCommand;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    /// Initialize the web seed manager if web seeds are configured.
    pub fn init_web_seed_manager(&mut self, piece_length: u32, total_length: u64) {
        if !self.web_seed_urls.is_empty() && self.web_seed_manager.is_none() {
            info!(
                count = self.web_seed_urls.len(),
                "Initializing web seed manager with {} URL(s)",
                self.web_seed_urls.len()
            );
            let tls = {
                let group = self.group.recover();
                crate::http::client_identity::ClientTlsConfig::from_download_options(
                    group.options(),
                )
            };
            match crate::engine::bt_web_seed::WebSeedManager::new_with_tls(
                self.web_seed_urls.clone(),
                piece_length,
                total_length,
                &tls,
            ) {
                Ok(manager) => self.web_seed_manager = Some(manager),
                Err(error) => tracing::error!(
                    error = %error,
                    "Failed to configure web-seed HTTP clients"
                ),
            }
        }
    }

    /// Get a reference to the web seed manager.
    pub fn get_web_seed_manager(&self) -> Option<&crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_ref()
    }

    /// Get a mutable reference to the web seed manager.
    pub fn get_web_seed_manager_mut(
        &mut self,
    ) -> Option<&mut crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_mut()
    }

    /// Check if web seeds are available.
    pub fn has_web_seeds(&self) -> bool {
        !self.web_seed_urls.is_empty()
    }

    /// Get web seed download statistics.
    pub fn web_seed_stats(&self) -> Option<&crate::engine::bt_web_seed::WebSeedStats> {
        self.web_seed_manager.as_ref().map(|m| m.stats())
    }
}
