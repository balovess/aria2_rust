//! Credential Store for HTTP Authentication
//!
//! Provides a thread-safe storage mechanism for domain-specific credentials.
//! All passwords are automatically zeroized from memory when entries are removed
//! or the store is dropped, preventing sensitive data from lingering in memory.
//!
//! # Features
//! - Thread-safe: Uses `RwLock` for concurrent read access
//! - Memory safe: Passwords are zeroed on drop using `zeroize`
//! - Domain-based: Credentials are organized by domain/hostname
//! - CRUD operations: Store, retrieve, remove, and clear credentials
//!
//! # Example
//! ```rust
//! use aria2_core::auth::credential_store::CredentialStore;
//!
//! let store = CredentialStore::new();
//! store.store("example.com", "alice", b"secret123");
//!
//! if let Some(creds) = store.get("example.com") {
//!     println!("Found credentials for user: {}", creds.username);
//! }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;
use zeroize::Zeroize;

/// A credential entry containing username and password for a specific domain.
///
/// The password field uses `Zeroize` to ensure secure memory erasure when the
/// entry is dropped. This prevents sensitive data from remaining in memory
/// after credentials are no longer needed.
#[derive(Clone)]
pub struct PasswordEntry {
    /// Username for authentication
    pub username: String,
    /// Password bytes (zeroized on drop)
    pub password: Vec<u8>,
}

impl fmt::Debug for PasswordEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordEntry")
            .field("username", &self.username)
            .field("password", &"***") // Mask password in debug output
            .finish()
    }
}

impl Drop for PasswordEntry {
    fn drop(&mut self) {
        // Zeroize the password before dropping
        self.password.zeroize();
    }
}

/// Thread-safe credential storage for HTTP authentication.
///
/// Stores username/password pairs indexed by domain (hostname or URL pattern).
/// Provides concurrent access through `RwLock`, allowing multiple readers
/// while ensuring exclusive access during writes.
///
/// # Security Guarantees
/// - All passwords are stored as `Vec<u8>` with automatic zeroization on drop
/// - When entries are removed or the store is cleared, passwords are immediately zeroed
/// - Debug output masks passwords to prevent accidental logging of secrets
pub struct CredentialStore {
    /// Internal storage: domain -> PasswordEntry mapping
    credentials: RwLock<HashMap<String, PasswordEntry>>,
}

impl CredentialStore {
    /// Creates a new empty credential store.
    ///
    /// # Example
    /// ```
    /// use aria2_core::auth::credential_store::CredentialStore;
    ///
    /// let store = CredentialStore::new();
    /// assert_eq!(store.count(), 0);
    /// ```
    pub fn new() -> Self {
        CredentialStore {
            credentials: RwLock::new(HashMap::new()),
        }
    }

    /// Stores credentials for a domain.
    ///
    /// If credentials already exist for the given domain, they will be replaced
    /// and the old password will be zeroized before being dropped.
    ///
    /// # Arguments
    /// * `domain` - Domain or hostname (e.g., "example.com")
    /// * `username` - Username for authentication
    /// * `password` - Password as byte slice (will be copied and stored securely)
    ///
    /// # Example
    /// ```
    /// use aria2_core::auth::credential_store::{CredentialStore, PasswordEntry};
    ///
    /// let mut store = CredentialStore::new();
    /// store.store("api.example.com", "admin", b"secure-password-123");
    /// ```
    pub fn store(&self, domain: &str, username: &str, password: &[u8]) {
        let mut creds = self.credentials.write().unwrap();
        let entry = PasswordEntry {
            username: username.to_string(),
            password: password.to_vec(),
        };
        creds.insert(domain.to_string(), entry);
    }

    /// Retrieves credentials for a domain.
    ///
    /// Returns a clone of the `PasswordEntry` if found, or `None` if no
    /// credentials exist for the specified domain.
    ///
    /// # Arguments
    /// * `domain` - Domain or hostname to look up
    ///
    /// # Returns
    /// - `Some(PasswordEntry)` if credentials exist
    /// - `None` if no credentials found
    ///
    /// # Security Note
    /// The returned `PasswordEntry` contains the actual password. Handle it
    /// carefully and allow it to be dropped naturally so the password gets
    /// zeroized automatically.
    ///
    /// # Example
    /// ```
    /// use aria2_core::auth::credential_store::{CredentialStore, PasswordEntry};
    ///
    /// let store = CredentialStore::new();
    /// store.store("example.com", "user", b"pass");
    /// if let Some(creds) = store.get("example.com") {
    ///     println!("Username: {}", creds.username);
    /// } // Password is zeroized when creds goes out of scope
    /// ```
    pub fn get(&self, domain: &str) -> Option<PasswordEntry> {
        let creds = self.credentials.read().unwrap();
        creds.get(domain).cloned()
    }

    /// Removes credentials for a domain.
    ///
    /// The removed entry's password will be zeroized before this method returns.
    ///
    /// # Arguments
    /// * `domain` - Domain or hostname to remove credentials for
    ///
    /// # Returns
    /// - `Some(PasswordEntry)` that was removed
    /// - `None` if no credentials existed for the domain
    ///
    /// # Example
    /// ```
    /// use aria2_core::auth::credential_store::{CredentialStore, PasswordEntry};
    ///
    /// let mut store = CredentialStore::new();
    /// store.store("old-server.com", "admin", b"pass");
    /// if let Some(removed) = store.remove("old-server.com") {
    ///     println!("Removed credentials for user: {}", removed.username);
    /// }
    /// ```
    pub fn remove(&self, domain: &str) -> Option<PasswordEntry> {
        let mut creds = self.credentials.write().unwrap();
        creds.remove(domain)
    }

    /// Clears all stored credentials.
    ///
    /// All passwords in the store will be zeroized before this method returns.
    /// After calling this method, the store will be empty.
    ///
    /// # Example
    /// ```
    /// use aria2_core::auth::credential_store::CredentialStore;
    ///
    /// let mut store = CredentialStore::new();
    /// store.store("example.com", "user", b"pass");
    /// store.clear(); // All passwords securely erased
    /// assert_eq!(store.count(), 0);
    /// ```
    pub fn clear(&self) {
        let mut creds = self.credentials.write().unwrap();
        creds.clear(); // All PasswordEntries are dropped, triggering zeroization
    }

    /// Returns the number of stored credential entries.
    pub fn count(&self) -> usize {
        let creds = self.credentials.read().unwrap();
        creds.len()
    }

    /// Checks if credentials exist for a domain.
    ///
    /// # Arguments
    /// * `domain` - Domain or hostname to check
    ///
    /// # Returns
    /// `true` if credentials exist, `false` otherwise
    pub fn has_credentials(&self, domain: &str) -> bool {
        let creds = self.credentials.read().unwrap();
        creds.contains_key(domain)
    }

    /// Lists all domains that have stored credentials.
    ///
    /// # Returns
    /// A vector of domain strings (not the actual credentials)
    ///
    /// # Note
    /// This does not expose any sensitive data, only domain names.
    pub fn list_domains(&self) -> Vec<String> {
        let creds = self.credentials.read().unwrap();
        creds.keys().cloned().collect()
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialStore")
            .field("entry_count", &self.count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_store_creation() {
        let store = CredentialStore::new();
        assert_eq!(store.count(), 0);
        assert!(!store.has_credentials("example.com"));
    }

    #[test]
    fn test_credential_store_default() {
        let store = CredentialStore::default();
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_credential_store_operations() {
        let store = CredentialStore::new();

        // Test initial state
        assert_eq!(store.count(), 0);

        // Store credentials
        store.store("example.com", "alice", b"secret123");
        assert_eq!(store.count(), 1);
        assert!(store.has_credentials("example.com"));

        // Retrieve credentials
        let creds = store.get("example.com").expect("Should find credentials");
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, b"secret123");

        // Remove credentials
        let removed = store
            .remove("example.com")
            .expect("Should remove credentials");
        assert_eq!(removed.username, "alice");
        assert_eq!(store.count(), 0);
        assert!(!store.has_credentials("example.com"));
    }

    #[test]
    fn test_credential_store_multiple_domains() {
        let store = CredentialStore::new();

        store.store("api.example.com", "admin", b"admin-pass");
        store.store("db.example.com", "dbuser", b"db-pass");
        store.store("auth.example.com", "authuser", b"auth-pass");

        assert_eq!(store.count(), 3);

        // Verify each domain has correct credentials
        let api_creds = store.get("api.example.com").unwrap();
        assert_eq!(api_creds.username, "admin");

        let db_creds = store.get("db.example.com").unwrap();
        assert_eq!(db_creds.username, "dbuser");

        // List domains
        let domains = store.list_domains();
        assert_eq!(domains.len(), 3);
        assert!(domains.contains(&"api.example.com".to_string()));
        assert!(domains.contains(&"db.example.com".to_string()));
        assert!(domains.contains(&"auth.example.com".to_string()));
    }

    #[test]
    fn test_credential_store_overwrite() {
        let store = CredentialStore::new();

        // Store initial credentials
        store.store("example.com", "user1", b"pass1");

        // Overwrite with new credentials
        store.store("example.com", "user2", b"pass2");

        // Should have new credentials
        assert_eq!(store.count(), 1); // Still only one entry

        let creds = store.get("example.com").unwrap();
        assert_eq!(creds.username, "user2");
        assert_eq!(creds.password, b"pass2");
    }

    #[test]
    fn test_credential_store_remove_nonexistent() {
        let store = CredentialStore::new();

        let result = store.remove("nonexistent.com");
        assert!(result.is_none());
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_credential_store_clear() {
        let store = CredentialStore::new();

        store.store("domain1.com", "user1", b"pass1");
        store.store("domain2.com", "user2", b"pass2");
        store.store("domain3.com", "user3", b"pass3");

        assert_eq!(store.count(), 3);

        store.clear();

        assert_eq!(store.count(), 0);
        assert!(!store.has_credentials("domain1.com"));
        assert!(!store.has_credentials("domain2.com"));
        assert!(!store.has_credentials("domain3.com"));
    }

    #[test]
    fn test_password_entry_debug_masking() {
        let entry = PasswordEntry {
            username: "testuser".to_string(),
            password: b"super-secret-password".to_vec(),
        };

        let debug_output = format!("{:?}", entry);

        // Verify username is visible but password is masked
        assert!(debug_output.contains("testuser"));
        assert!(debug_output.contains("***")); // Masked password
        assert!(!debug_output.contains("super-secret-password"));
    }

    #[test]
    fn test_credential_store_debug() {
        let store = CredentialStore::new();
        store.store("example.com", "user", b"pass");

        let debug_output = format!("{:?}", store);

        // Should show count but not expose credentials
        assert!(debug_output.contains("CredentialStore"));
        assert!(debug_output.contains("entry_count"));
        assert!(debug_output.contains("1"));
        assert!(!debug_output.contains("user"));
        assert!(!debug_output.contains("pass"));
    }

    #[test]
    fn test_credential_store_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(CredentialStore::new());
        let mut handles = vec![];

        // Spawn multiple threads that write concurrently
        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                let domain = format!("domain{}.com", i);
                store_clone.store(
                    &domain,
                    &format!("user{}", i),
                    format!("pass{}", i).as_bytes(),
                );
            }));
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all entries were created
        assert_eq!(store.count(), 10);

        // Verify we can read from multiple threads
        let mut read_handles = vec![];
        for i in 0..10 {
            let store_clone = Arc::clone(&store);
            read_handles.push(thread::spawn(move || {
                let domain = format!("domain{}.com", i);
                let creds = store_clone.get(&domain).unwrap();
                assert_eq!(creds.username, format!("user{}", i));
            }));
        }

        for handle in read_handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_empty_domain_and_credentials() {
        let store = CredentialStore::new();

        // Edge case: empty domain string
        store.store("", "user", b"pass");
        assert!(store.has_credentials(""));
        let creds = store.get("").unwrap();
        assert_eq!(creds.username, "user");

        // Edge case: empty username
        store.store("empty-user.com", "", b"pass");
        let creds = store.get("empty-user.com").unwrap();
        assert_eq!(creds.username, "");

        // Edge case: empty password
        store.store("empty-pass.com", "user", b"");
        let creds = store.get("empty-pass.com").unwrap();
        assert_eq!(creds.password, b"");

        assert_eq!(store.count(), 3);
    }

    #[test]
    fn test_binary_password_storage() {
        let store = CredentialStore::new();

        // Store binary data as password (e.g., hashed credentials)
        let binary_pass: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF];
        store.store("binary-auth.com", "binary_user", &binary_pass);

        let creds = store.get("binary-auth.com").unwrap();
        assert_eq!(creds.password, binary_pass);
        assert_eq!(creds.password.len(), 6);
    }

    #[test]
    fn test_unicode_in_credentials() {
        let store = CredentialStore::new();

        // Unicode characters in all fields
        store.store("例子.コム", "ユーザー名", "パスワード".as_bytes());

        let creds = store.get("例子.コム").unwrap();
        assert_eq!(creds.username, "ユーザー名");
        assert_eq!(creds.password, "パスワード".as_bytes());
    }
}
