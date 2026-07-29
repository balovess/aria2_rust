use tracing::info;

use super::BtDownloadCommand;

/// Web seed (BEP 19) integration API.
pub trait BtDownloadCommandWebSeedApi {
    fn init_web_seed_manager(&mut self, piece_length: u32, total_length: u64);
    fn get_web_seed_manager(&self) -> Option<&crate::engine::bt_web_seed::WebSeedManager>;
    fn get_web_seed_manager_mut(&mut self) -> Option<&mut crate::engine::bt_web_seed::WebSeedManager>;
    fn has_web_seeds(&self) -> bool;
    fn web_seed_stats(&self) -> Option<&crate::engine::bt_web_seed::WebSeedStats>;
}

impl BtDownloadCommandWebSeedApi for BtDownloadCommand {
    fn init_web_seed_manager(&mut self, piece_length: u32, total_length: u64) {
        if !self.web_seed_urls.is_empty() && self.web_seed_manager.is_none() {
            info!(
                count = self.web_seed_urls.len(),
                "Initializing web seed manager with {} URL(s)",
                self.web_seed_urls.len()
            );
            self.web_seed_manager = Some(crate::engine::bt_web_seed::WebSeedManager::new(
                self.web_seed_urls.clone(),
                piece_length,
                total_length,
            ));
        }
    }

    fn get_web_seed_manager(&self) -> Option<&crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_ref()
    }

    fn get_web_seed_manager_mut(&mut self) -> Option<&mut crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_mut()
    }

    fn has_web_seeds(&self) -> bool {
        !self.web_seed_urls.is_empty()
    }

    fn web_seed_stats(&self) -> Option<&crate::engine::bt_web_seed::WebSeedStats> {
        self.web_seed_manager.as_ref().map(|m| m.stats())
    }
}
