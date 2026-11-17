use super::netrc;
use std::fmt;

/// Simplified Netrc store — maps hostname to (login, password).
///
/// Built from the full [`netrc::NetrcParser`] via conversion. Machine entries
/// are stored in `entries`; the `default` entry (which matches any host) is
/// stored separately so that `find_with_fallback` can first try a machine
/// match then fall back to the default, matching aria2's `findAuthenticator`.
#[derive(Clone)]
pub struct NetrcStore {
    entries: Vec<NetrcEntry>,
    default_entry: Option<NetrcEntry>,
}

impl fmt::Debug for NetrcStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetrcStore")
            .field("entry_count", &self.entries.len())
            .field("has_default_entry", &self.default_entry.is_some())
            .finish()
    }
}

/// A single `.netrc` entry.
#[derive(Clone)]
pub struct NetrcEntry {
    /// Machine hostname.
    pub host: String,
    /// Login username.
    pub login: String,
    /// Login password.
    pub password: String,
}

impl fmt::Debug for NetrcEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetrcEntry")
            .field("host", &self.host)
            .field("login", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

impl NetrcStore {
    /// Create an empty Netrc store.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            default_entry: None,
        }
    }

    /// Create a Netrc store from a list of entries and an optional default.
    pub fn from_entries(entries: Vec<NetrcEntry>) -> Self {
        Self {
            entries,
            default_entry: None,
        }
    }

    /// Create a Netrc store with machine entries and a default entry.
    pub fn with_default(entries: Vec<NetrcEntry>, default: Option<NetrcEntry>) -> Self {
        Self {
            entries,
            default_entry: default,
        }
    }

    /// Look up credentials for a hostname (without default fallback).
    ///
    /// Machine names beginning with `.` match subdomains, but not the bare
    /// domain or a numeric host, as in aria2's `.netrc` implementation.
    pub fn find(&self, host: &str) -> Option<&NetrcEntry> {
        self.entries
            .iter()
            .find(|entry| netrc::no_proxy_domain_match(host, &entry.host))
    }

    /// Look up credentials for a hostname, falling back to the default entry.
    ///
    /// This mirrors aria2's `Netrc::findAuthenticator()` logic.
    pub fn find_with_fallback(&self, host: &str) -> Option<&NetrcEntry> {
        self.find(host).or(self.default_entry.as_ref())
    }

    /// The default entry, if any.
    pub fn default_entry(&self) -> Option<&NetrcEntry> {
        self.default_entry.as_ref()
    }

    /// Check whether any entries exist.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.default_entry.is_none()
    }
}

impl Default for NetrcStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `NetrcStore` from a parsed [`netrc::NetrcParser`].
///
/// This replaces the old `From<NetrcParser>` conversion (which could not
/// populate `default_entry`) with a proper builder that separates machine
/// entries from the default entry.
impl From<netrc::NetrcParser> for NetrcStore {
    fn from(parser: netrc::NetrcParser) -> Self {
        let mut entries = Vec::with_capacity(parser.entries().len());

        for entry in parser.entries() {
            if let (Some(login), Some(password)) = (&entry.login, &entry.password) {
                entries.push(NetrcEntry {
                    host: entry.machine.clone(),
                    login: login.clone(),
                    password: password.clone(),
                });
            }
        }

        let default_entry = parser.default_entry().and_then(|entry| {
            if let (Some(login), Some(password)) = (&entry.login, &entry.password) {
                Some(NetrcEntry {
                    host: String::new(),
                    login: login.clone(),
                    password: password.clone(),
                })
            } else {
                None
            }
        });

        Self {
            entries,
            default_entry,
        }
    }
}
