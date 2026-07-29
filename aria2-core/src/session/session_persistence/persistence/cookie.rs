//! Cookie persistence helpers for SessionPersistence.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::http::cookie_storage::CookieJar;

use super::types::SessionPersistence;

impl SessionPersistence {
    /// Save cookie jar to a JSON file for persistence.
    ///
    /// Serializes all cookies in the jar to JSON format for storage alongside
    /// session data. Uses simple JSON serialization since CookieJar doesn't
    /// have built-in file I/O methods.
    pub(super) async fn save_cookie_jar_to_file(
        jar: &CookieJar,
        path: &Path,
    ) -> Result<(), String> {
        // Use serde_json to serialize the cookie jar's internal data
        #[derive(Serialize)]
        struct SerializableJar<'a> {
            cookies: &'a [crate::http::cookie_storage::JarCookie],
        }

        let serializable = SerializableJar {
            cookies: &jar.cookies,
        };

        let json = serde_json::to_string_pretty(&serializable).map_err(|e| e.to_string())?;

        tokio::fs::write(path, json)
            .await
            .map_err(|e| format!("Failed to write cookie file: {}", e))
    }

    /// Load cookie jar from a JSON file.
    ///
    /// Deserializes cookies from JSON format and creates a new CookieJar
    /// instance with the loaded data.
    pub(super) async fn load_cookie_jar_from_file(path: &Path) -> Result<CookieJar, String> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Failed to read cookie file: {}", e))?;

        #[derive(Deserialize)]
        struct SerializableJar {
            cookies: Vec<crate::http::cookie_storage::JarCookie>,
        }

        let parsed: SerializableJar =
            serde_json::from_str(&content).map_err(|e| format!("Invalid cookie JSON: {}", e))?;

        let mut jar = CookieJar::new();
        for cookie in parsed.cookies {
            jar.store(cookie);
        }

        Ok(jar)
    }
}
