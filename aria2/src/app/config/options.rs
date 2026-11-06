use aria2_core::config::{OptionValue, project_initial_options};
use aria2_core::request::request_group::DownloadOptions;

use super::super::App;

impl App {
    pub(in crate::app) async fn global_option_values(
        &self,
    ) -> std::collections::HashMap<String, OptionValue> {
        let config = self.config.read().await;
        config.get_all_global_options().await
    }

    /// Return the typed execution options together with the canonical raw
    /// request values that must remain attached to each CLI-created
    /// `RequestGroup`.
    pub(in crate::app) async fn download_options_with_snapshot(
        &self,
    ) -> (
        DownloadOptions,
        std::collections::HashMap<String, serde_json::Value>,
    ) {
        let values = self.global_option_values().await;
        let options = DownloadOptions::from_option_values(&values);
        let snapshot = project_initial_options(
            values
                .into_iter()
                .filter(|(_, value)| !value.is_none())
                .map(|(name, value)| (name, serde_json::Value::from(&value))),
        );
        (options, snapshot)
    }
    /// Get a string option value.
    pub(in crate::app) async fn get_opt_str(&self, name: &str) -> Option<String> {
        self.config.read().await.get_global_str(name).await
    }

    /// Get an integer option value.
    pub(in crate::app) async fn get_opt_i64(&self, name: &str) -> Option<i64> {
        self.config.read().await.get_global_i64(name).await
    }

    /// Get an usize option value.
    pub(in crate::app) async fn get_opt_usize(&self, name: &str) -> Option<usize> {
        self.config
            .read()
            .await
            .get_global_i64(name)
            .await
            .map(|v| v as usize)
    }

    /// Get a boolean option value.
    pub(in crate::app) async fn get_opt_bool(&self, name: &str) -> Option<bool> {
        self.config.read().await.get_global_bool(name).await
    }
}
