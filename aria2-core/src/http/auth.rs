//! HTTP authentication configuration and credential resolution.
//!
//! Provides `AuthConfig` (user:password pair), `BasicCred` (cached credential
//! scoped to host/port/path), and `AuthConfigFactory` (resolves credentials
//! for a request using URL-embedded creds, BasicCred cache, Netrc, and
//! defaults — matching the C++ `AuthConfig` + `AuthConfigFactory` design).

pub mod netrc;

use std::collections::BTreeSet;
use std::path::Path;

use tracing::{debug, info, warn};
use url::Url;

pub(crate) fn request_directory(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    }
}

use crate::http::request_response::basic_auth;

// ---------------------------------------------------------------------------
// AuthConfig — mirrors C++ AuthConfig
// ---------------------------------------------------------------------------

/// A resolved user:password pair ready for inclusion in an Authorization header.
///
/// Mirrors the C++ `AuthConfig` class.  Call [`AuthConfig::auth_text`] to get
/// the `"user:password"` string, or pass it to `basic_auth()` /
/// `DigestAuthResponse::compute()` for the actual header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    user: String,
    password: String,
}

impl AuthConfig {
    /// Create a new `AuthConfig`.  Returns `None` if `user` is empty,
    /// matching the C++ `AuthConfig::create()` semantics.
    pub fn new(user: String, password: String) -> Option<Self> {
        if user.is_empty() {
            None
        } else {
            Some(Self { user, password })
        }
    }

    /// The username.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The password.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Returns `"user:password"`, matching C++ `getAuthText()`.
    pub fn auth_text(&self) -> String {
        format!("{}:{}", self.user, self.password)
    }
}

impl std::fmt::Display for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.user, self.password)
    }
}
// ---------------------------------------------------------------------------
// BasicCred — mirrors C++ BasicCred
// ---------------------------------------------------------------------------

/// A cached Basic-auth credential scoped to a host:port + path prefix.
///
/// Matching the C++ `BasicCred`, the path is normalised to always end with
/// `/` so that prefix-matching works correctly.
#[derive(Debug, Clone)]
pub struct BasicCred {
    /// Username
    pub user: String,
    /// Password
    pub password: String,
    /// Hostname this credential applies to
    pub host: String,
    /// Port number
    pub port: u16,
    /// Path prefix (always ends with '/')
    pub path: String,
    /// Whether this credential has been activated by a 401 challenge
    pub activated: bool,
}

impl BasicCred {
    /// Create a new `BasicCred`, normalising the path to end with `/`.
    pub fn new(
        user: String,
        password: String,
        host: String,
        port: u16,
        path: String,
        activated: bool,
    ) -> Self {
        let mut path = path;
        if path.is_empty() || !path.ends_with('/') {
            path.push('/');
        }
        Self {
            user,
            password,
            host,
            port,
            path,
            activated,
        }
    }

    /// Activate this credential (called when a 401 challenge is received).
    pub fn activate(&mut self) {
        self.activated = true;
    }

    /// Whether this credential is activated.
    pub fn is_activated(&self) -> bool {
        self.activated
    }
}

impl PartialEq for BasicCred {
    fn eq(&self, other: &Self) -> bool {
        self.host == other.host && self.port == other.port && self.path == other.path
    }
}

impl Eq for BasicCred {}

impl PartialOrd for BasicCred {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BasicCred {
    /// Ordering matches the C++ `operator<`:
    /// host < host, then port < port, then path > path (longer paths first).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.host.cmp(&other.host) {
            std::cmp::Ordering::Equal => match self.port.cmp(&other.port) {
                std::cmp::Ordering::Equal => other.path.cmp(&self.path), // reversed
                ord => ord,
            },
            ord => ord,
        }
    }
}
// ---------------------------------------------------------------------------
// AuthConfigFactory — mirrors C++ AuthConfigFactory
// ---------------------------------------------------------------------------

/// Default FTP credentials, matching C++ `AUTH_DEFAULT_USER` / `AUTH_DEFAULT_PASSWD`.
const FTP_DEFAULT_USER: &str = "anonymous";
const FTP_DEFAULT_PASSWD: &str = "ARIA2USER@";

/// Resolves [`AuthConfig`] for a given request URL, following the C++ aria2
/// resolution chain:
///
/// 1. URL-embedded credentials (`http://user:pass@host/...`)
/// 2. Activated `BasicCred` cache (when `http_auth_challenge` is enabled)
/// 3. HTTP CLI options, then HTTP machine-specific Netrc lookup
/// 4. FTP machine/default Netrc lookup, then FTP CLI options
/// 5. FTP anonymous default (`anonymous` / `ARIA2USER@`)
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::auth::AuthConfigFactory;
///
/// let mut factory = AuthConfigFactory::new();
/// let url = url::Url::parse("http://user:pass@example.com/file").unwrap();
/// let auth = factory.resolve(&url, false, None, None, false);
/// assert!(auth.is_some());
/// ```
#[derive(Debug)]
pub struct AuthConfigFactory {
    /// Cached Basic-auth credentials, ordered by (host, port, path-reversed).
    basic_creds: BTreeSet<BasicCred>,
    /// Parsed .netrc entries (host -> (login, password)).
    netrc: Option<NetrcStore>,
}

/// Simplified Netrc store — maps hostname to (login, password).
///
/// Built from the full [`netrc::NetrcParser`] via conversion. Machine entries
/// are stored in `entries`; the `default` entry (which matches any host) is
/// stored separately so that [`find_with_fallback`] can first try a machine
/// match then fall back to the default, matching C++ `findAuthenticator()`.
#[derive(Debug, Clone)]
pub struct NetrcStore {
    entries: Vec<NetrcEntry>,
    default_entry: Option<NetrcEntry>,
}

/// A single .netrc entry.
#[derive(Debug, Clone)]
pub struct NetrcEntry {
    /// Machine hostname
    pub host: String,
    /// Login username
    pub login: String,
    /// Login password
    pub password: String,
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
            .find(|e| netrc::no_proxy_domain_match(host, &e.host))
    }

    /// Look up credentials for a hostname, falling back to the default entry.
    ///
    /// This mirrors the C++ `Netrc::findAuthenticator()` logic.
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

        let default_entry = parser.default_entry().and_then(|d| {
            if let (Some(login), Some(password)) = (&d.login, &d.password) {
                Some(NetrcEntry {
                    host: String::new(),
                    login: login.clone(),
                    password: password.clone(),
                })
            } else {
                None
            }
        });

        NetrcStore {
            entries,
            default_entry,
        }
    }
}

/// Options that influence auth resolution, passed per-request.
///
/// These correspond to C++ `Option` prefs like `PREF_HTTP_USER`,
/// `PREF_HTTP_PASSWD`, `PREF_NO_NETRC`, and `PREF_HTTP_AUTH_CHALLENGE`.
#[derive(Debug, Clone, Default)]
pub struct AuthResolveOptions {
    /// Whether HTTP auth-challenge mode is enabled (C++ `PREF_HTTP_AUTH_CHALLENGE`).
    /// When true, 401 responses trigger BasicCred activation.
    pub http_auth_challenge: bool,

    /// Whether to skip Netrc lookups (C++ `PREF_NO_NETRC`).
    pub no_netrc: bool,

    /// CLI-specified HTTP username (C++ `PREF_HTTP_USER`).
    pub http_user: Option<String>,
    /// CLI-specified HTTP password (C++ `PREF_HTTP_PASSWD`).
    pub http_passwd: Option<String>,

    /// CLI-specified FTP username (C++ `PREF_FTP_USER`).
    pub ftp_user: Option<String>,
    /// CLI-specified FTP password (C++ `PREF_FTP_PASSWD`).
    pub ftp_passwd: Option<String>,
    /// Credentials for an HTTP proxy protection space.
    pub proxy_user: Option<String>,
    pub proxy_passwd: Option<String>,
}

impl AuthConfigFactory {
    /// Create a new factory with no cached credentials or Netrc.
    pub fn new() -> Self {
        Self {
            basic_creds: BTreeSet::new(),
            netrc: None,
        }
    }

    /// Set the Netrc store.
    pub fn set_netrc(&mut self, netrc: NetrcStore) {
        self.netrc = Some(netrc);
    }

    /// Load netrc credentials from a file path.
    ///
    /// Parses the file using [`netrc::NetrcParser`] and converts it into
    /// a [`NetrcStore`] for use by the auth resolution chain.
    pub fn load_netrc_file(&mut self, path: &Path) -> Result<(), netrc::NetrcError> {
        let parser = netrc::NetrcParser::parse_file(path)?;
        self.netrc = Some(NetrcStore::from(parser));
        Ok(())
    }

    /// Load netrc credentials from a string.
    ///
    /// Useful for testing or when the netrc content comes from a source
    /// other than a file.
    pub fn load_netrc_str(&mut self, content: &str) -> Result<(), netrc::NetrcError> {
        let parser = netrc::NetrcParser::parse(content)?;
        self.netrc = Some(NetrcStore::from(parser));
        Ok(())
    }

    /// Resolve proxy credentials without consulting origin credentials.
    pub fn resolve_proxy(&self, opts: &AuthResolveOptions) -> Option<AuthConfig> {
        let user = opts.proxy_user.clone()?;
        AuthConfig::new(user, opts.proxy_passwd.clone().unwrap_or_default())
    }

    /// Resolve the origin credentials that aria2 sends preemptively as Basic.
    ///
    /// Challenge mode intentionally returns None until URL credentials or an
    /// activated BasicCred exists. Non-challenge mode follows the normal
    /// URL/CLI/Netrc resolver chain. Digest remains challenge-driven and is
    /// handled by the response path.
    pub fn resolve_basic_authorization(
        &mut self,
        url: &Url,
        opts: &AuthResolveOptions,
    ) -> Option<String> {
        self.resolve(url, url.password().is_some(), opts)
            .map(|config| basic_auth(config.user(), config.password()))
    }

    /// Resolve the [`AuthConfig`] for the given request URL.
    ///
    /// This mirrors the C++ `AuthConfigFactory::createAuthConfig()` logic,
    /// branching on protocol (http/https vs ftp/sftp).
    pub fn resolve(
        &mut self,
        url: &Url,
        has_password: bool,
        opts: &AuthResolveOptions,
    ) -> Option<AuthConfig> {
        let scheme = url.scheme();
        if scheme == "http" || scheme == "https" {
            self.resolve_http(url, has_password, opts)
        } else if scheme == "ftp" || scheme == "sftp" {
            self.resolve_ftp(url, has_password, opts)
        } else {
            None
        }
    }

    // -- HTTP / HTTPS resolution -----------------------------------------

    fn resolve_http(
        &mut self,
        url: &Url,
        _has_password: bool,
        opts: &AuthResolveOptions,
    ) -> Option<AuthConfig> {
        let username = url.username();
        let password = url.password().unwrap_or("");
        let host = url.host_str().unwrap_or("");
        let port = url.port_or_known_default().unwrap_or(80);

        if opts.http_auth_challenge {
            // Challenge mode: URL creds -> BasicCred cache -> null
            if !username.is_empty() {
                let path = request_directory(url.path());
                self.update_basic_cred(BasicCred::new(
                    username.to_string(),
                    password.to_string(),
                    host.to_string(),
                    port,
                    path.to_string(),
                    true,
                ));
                return AuthConfig::new(username.to_string(), password.to_string());
            }
            // Look up activated BasicCred
            let cred = self.find_basic_cred(host, port, request_directory(url.path()));
            match cred {
                Some(bc) => AuthConfig::new(bc.user.clone(), bc.password.clone()),
                None => None,
            }
        } else {
            // Non-challenge mode: URL creds -> resolver chain
            if !username.is_empty() {
                return AuthConfig::new(username.to_string(), password.to_string());
            }
            self.resolve_http_via_chain(host, opts)
        }
    }

    /// Resolve HTTP auth via CLI-option / Netrc chain (non-challenge mode).
    fn resolve_http_via_chain(&self, host: &str, opts: &AuthResolveOptions) -> Option<AuthConfig> {
        // Explicit HTTP options take precedence over machine entries. This
        // keeps a task's explicit credentials from being shadowed by ambient
        // process configuration in `.netrc`.
        if let Some(ref user) = opts.http_user
            && !user.is_empty()
        {
            debug!("Resolved HTTP auth for {} from CLI options", host);
            return AuthConfig::new(user.clone(), opts.http_passwd.clone().unwrap_or_default());
        }

        // HTTP auth intentionally ignores the .netrc `default` entry, matching
        // C++ createHttpAuthResolver()->ignoreDefault().
        if !opts.no_netrc
            && let Some(ref netrc) = self.netrc
            && let Some(entry) = netrc.find(host)
        {
            debug!(
                "Resolved HTTP auth for {} from Netrc (user={})",
                host, entry.login
            );
            return AuthConfig::new(entry.login.clone(), entry.password.clone());
        }
        None
    }

    // -- FTP / SFTP resolution -------------------------------------------

    fn resolve_ftp(
        &mut self,
        url: &Url,
        has_password: bool,
        opts: &AuthResolveOptions,
    ) -> Option<AuthConfig> {
        let username = url.username();
        let host = url.host_str().unwrap_or("");

        if !username.is_empty() {
            if has_password {
                let password = url.password().unwrap_or("");
                return AuthConfig::new(username.to_string(), password.to_string());
            }
            // URL has username but no password — try Netrc first
            if !opts.no_netrc
                && let Some(ref netrc) = self.netrc
                && let Some(entry) = netrc.find(host)
                && entry.login == username
            {
                return AuthConfig::new(entry.login.clone(), entry.password.clone());
            }
            // Fall back to CLI FTP password
            let ftp_passwd = opts.ftp_passwd.clone().unwrap_or_default();
            return AuthConfig::new(username.to_string(), ftp_passwd);
        }

        // No URL username — resolve via chain (Netrc -> CLI -> anonymous)
        self.resolve_ftp_via_chain(host, opts)
    }

    /// Resolve FTP auth via Netrc / CLI-option / anonymous-default chain.
    fn resolve_ftp_via_chain(&self, host: &str, opts: &AuthResolveOptions) -> Option<AuthConfig> {
        // Netrc lookup with default fallback
        if !opts.no_netrc
            && let Some(ref netrc) = self.netrc
            && let Some(entry) = netrc.find_with_fallback(host)
        {
            debug!(
                "Resolved FTP auth for {} from Netrc (user={})",
                host, entry.login
            );
            return AuthConfig::new(entry.login.clone(), entry.password.clone());
        }
        // CLI fallback
        if let Some(ref user) = opts.ftp_user
            && !user.is_empty()
        {
            let passwd = opts.ftp_passwd.clone().unwrap_or_default();
            return AuthConfig::new(user.clone(), passwd);
        }
        // FTP anonymous default
        debug!("Resolved FTP auth for {} as anonymous (default)", host);
        AuthConfig::new(FTP_DEFAULT_USER.to_string(), FTP_DEFAULT_PASSWD.to_string())
    }

    // -- BasicCred management --------------------------------------------

    /// Update or insert a `BasicCred`.  If one with the same host/port/path
    /// already exists, its user/password/activated fields are replaced.
    /// Mirrors C++ `updateBasicCred()`.
    pub fn update_basic_cred(&mut self, cred: BasicCred) {
        if let Some(existing) = self.basic_creds.take(&cred) {
            // Replace with new credential data (same key, updated values)
            let mut updated = cred;
            // Preserve activation if the new one is not activated but old was
            if !updated.activated && existing.activated {
                updated.activated = true;
            }
            self.basic_creds.insert(updated);
            debug!(
                "Updated BasicCred for {}:{}{}",
                existing.host, existing.port, existing.path
            );
        } else {
            debug!(
                "Inserted BasicCred for {}:{}{}",
                cred.host, cred.port, cred.path
            );
            self.basic_creds.insert(cred);
        }
    }

    /// Activate a `BasicCred` for the given host/port/path.
    ///
    /// If found, activate it.  If not found, attempt to resolve credentials
    /// via the HTTP auth chain and create a new activated `BasicCred`.
    /// Returns `true` if activation succeeded.
    /// Mirrors C++ `activateBasicCred()`.
    pub fn activate_basic_cred(
        &mut self,
        host: &str,
        port: u16,
        path: &str,
        opts: &AuthResolveOptions,
    ) -> bool {
        // Check if a matching cred exists
        let found = self.find_basic_cred(host, port, path).is_some();

        if found {
            self.find_basic_cred_mut(host, port, path, |cred| {
                cred.activate();
            });
            info!("Activated existing BasicCred for {}:{}{}", host, port, path);
            true
        } else {
            // Try to resolve from chain
            let auth = self.resolve_http_via_chain(host, opts);
            match auth {
                Some(ac) => {
                    self.basic_creds.insert(BasicCred::new(
                        ac.user().to_string(),
                        ac.password().to_string(),
                        host.to_string(),
                        port,
                        "/".to_string(),
                        true,
                    ));
                    info!(
                        "Created and activated BasicCred for {}:{}{}",
                        host, port, path
                    );
                    true
                }
                None => {
                    warn!(
                        "Cannot activate BasicCred for {}:{}{} — no credentials found",
                        host, port, path
                    );
                    false
                }
            }
        }
    }

    /// Find a `BasicCred` matching host/port/path (path prefix matching).
    /// Mirrors C++ `findBasicCred()`.
    pub fn find_basic_cred(&self, host: &str, port: u16, path: &str) -> Option<&BasicCred> {
        let search = BasicCred::new(
            String::new(),
            String::new(),
            host.to_string(),
            port,
            path.to_string(),
            false,
        );
        let search_path = search.path.clone();
        for cred in self.basic_creds.range(search..) {
            if cred.host != host || cred.port != port {
                break;
            }
            // Path prefix matching: request path must start with cred path
            if search_path.starts_with(&cred.path) || search_path == cred.path {
                return Some(cred);
            }
        }
        None
    }

    /// Find a mutable `BasicCred` matching host/port/path.
    ///
    /// Since `BTreeSet` does not provide mutable access by key, this
    /// method removes the matching entry, applies the mutation callback,
    /// and re-inserts it.
    fn find_basic_cred_mut<F>(&mut self, host: &str, port: u16, path: &str, mut mutate: F)
    where
        F: FnMut(&mut BasicCred),
    {
        if let Some(cred) = self.find_basic_cred(host, port, path).cloned() {
            self.basic_creds.remove(&cred);
            let mut modified = cred;
            mutate(&mut modified);
            self.basic_creds.insert(modified);
        }
    }

    /// Number of cached BasicCred entries.
    pub fn basic_cred_count(&self) -> usize {
        self.basic_creds.len()
    }
}

impl Default for AuthConfigFactory {
    fn default() -> Self {
        Self::new()
    }
}
// ---------------------------------------------------------------------------
// Confidential-info erasure
// ---------------------------------------------------------------------------

/// Strip confidential header values from an HTTP request/response string
/// for safe logging.  Replaces values of Authorization, Proxy-Authorization,
/// Cookie, and Set-Cookie headers with `<snip>`.
///
/// Mirrors C++ `HttpConnection::eraseConfidentialInfo()`.
pub fn erase_confidential_info(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    for line in raw.lines() {
        if line.len() >= 15 && line[..15].eq_ignore_ascii_case("authorization: ") {
            result.push_str("Authorization: <snip>\n");
        } else if line.len() >= 21 && line[..21].eq_ignore_ascii_case("proxy-authorization: ") {
            result.push_str("Proxy-Authorization: <snip>\n");
        } else if line.len() >= 8 && line[..8].eq_ignore_ascii_case("cookie: ") {
            result.push_str("Cookie: <snip>\n");
        } else if line.len() >= 12 && line[..12].eq_ignore_ascii_case("set-cookie: ") {
            result.push_str("Set-Cookie: <snip>\n");
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}
