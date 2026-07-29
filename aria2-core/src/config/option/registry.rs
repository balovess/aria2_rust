//! Option registry — the central store for all aria2 configuration options.

use std::collections::HashMap;

use super::types::{OptionCategory, OptionDef};

/// Registry of all known configuration options.
#[derive(Clone)]
pub struct OptionRegistry {
    pub(super) options: HashMap<String, OptionDef>,
}

impl OptionRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            options: HashMap::new(),
        };
        reg.register_general_options();
        reg.register_http_ftp_options();
        #[cfg(feature = "bittorrent")]
        reg.register_bt_options();
        reg.register_rpc_options();
        reg.register_advanced_options();
        reg
    }

    pub fn register(&mut self, def: OptionDef) {
        self.options.insert(def.name().to_string(), def);
    }

    pub fn get(&self, name: &str) -> Option<&OptionDef> {
        self.options.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    pub fn all(&self) -> &HashMap<String, OptionDef> {
        &self.options
    }

    pub fn count(&self) -> usize {
        self.options.len()
    }

    pub fn by_category(&self, cat: OptionCategory) -> Vec<&OptionDef> {
        self.options
            .values()
            .filter(|d| d.get_category() == cat)
            .collect()
    }
}

impl Default for OptionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
