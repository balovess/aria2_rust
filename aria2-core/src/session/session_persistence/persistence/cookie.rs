//! Cookie persistence helpers for SessionPersistence.

use std::path::Path;

use crate::http::cookie::CookieStorage;

use super::types::SessionPersistence;

impl SessionPersistence {
    /// Save canonical CookieStorage using the aria2 Netscape cookie format.
    pub(super) async fn save_cookie_storage_to_file(
        storage: &CookieStorage,
        path: &Path,
    ) -> Result<(), String> {
        storage.save_file(path).map_err(|e| e.to_string())
    }

    /// Load canonical CookieStorage from an aria2 Netscape cookie file.
    pub(super) async fn load_cookie_storage_from_file(
        storage: &CookieStorage,
        path: &Path,
    ) -> Result<(), String> {
        storage
            .load_file(path)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
