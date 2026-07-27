//! Netrc file parser for ~/.netrc credentials.
//!
//! Parses the standard `.netrc` file format used by FTP/HTTP clients to store
//! credentials per-machine. This is a Rust port of the C++ `Netrc` class from
//! aria2, using the same state-machine parsing approach.
//!
//! # Supported tokens
//!
//! - `machine <name>` — identify a remote machine
//! - `login <name>` — user name for the machine
//! - `password <string>` — password for the machine
//! - `account <string>` — account (rarely used)
//! - `default` — matches any host without an explicit entry (must come last)
//! - `macdef <name>` — macro definition (skipped, not needed by aria2)
//!
//! # Format rules
//!
//! - Lines starting with `#` are comments
//! - Tokens are whitespace-separated
//! - Keywords are case-insensitive (machine/MACHINE both valid)
//! - `default` entry is optional and must come last
//!
//! # Example
//!
//! ```rust,ignore
//! use aria2_core::http::auth::netrc::NetrcParser;
//!
//! let content = "machine ftp.example.com\nlogin myuser\npassword mypass\n";
//! let parser = NetrcParser::parse(content).unwrap();
//! let entry = parser.find("ftp.example.com");
//! assert!(entry.is_some());
//! ```

use std::fmt;
use std::path::Path;

use tracing::debug;

// ---------------------------------------------------------------------------
// NetrcEntry — a single .netrc entry for a specific machine
// ---------------------------------------------------------------------------

/// A single Netrc entry for a specific machine.
///
/// Mirrors the C++ `Authenticator` class. Each entry holds credentials
/// scoped to a machine hostname. The `default` entry uses an empty `machine`
/// string and matches any host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetrcEntry {
    /// The machine hostname. Empty for the `default` entry.
    pub machine: String,
    /// The login username.
    pub login: Option<String>,
    /// The password.
    pub password: Option<String>,
    /// The account (rarely used).
    pub account: Option<String>,
}

impl NetrcEntry {
    /// Create a new entry for the given machine name.
    pub fn new(machine: String) -> Self {
        Self {
            machine,
            login: None,
            password: None,
            account: None,
        }
    }

    /// Create a default entry that matches any host.
    pub fn new_default() -> Self {
        Self {
            machine: String::new(),
            login: None,
            password: None,
            account: None,
        }
    }

    /// Whether this entry has both login and password set.
    pub fn has_credentials(&self) -> bool {
        self.login.is_some() && self.password.is_some()
    }

    /// Whether this is a default (catch-all) entry.
    pub fn is_default(&self) -> bool {
        self.machine.is_empty()
    }
}

// ---------------------------------------------------------------------------
// NetrcError — parse / I/O errors
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing a `.netrc` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetrcError {
    /// The file could not be found at the specified path.
    FileNotFound(String),
    /// An I/O error occurred while reading the file.
    IoError(String),
    /// A parse error: unexpected token or premature EOF.
    ParseError(String),
}

impl fmt::Display for NetrcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotFound(p) => write!(f, "netrc file not found: {}", p),
            Self::IoError(e) => write!(f, "netrc I/O error: {}", e),
            Self::ParseError(e) => write!(f, "netrc parse error: {}", e),
        }
    }
}

impl std::error::Error for NetrcError {}

// ---------------------------------------------------------------------------
// Parser state machine — mirrors C++ Netrc::parse STATE enum
// ---------------------------------------------------------------------------

/// Internal parser state, matching the C++ `STATE` enum exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// Expecting a keyword token (machine, default, login, password, …)
    GetToken,
    /// Next token is the machine hostname value.
    SetMachine,
    /// Next token is the login username value.
    SetLogin,
    /// Next token is the password value.
    SetPassword,
    /// Next token is the account value.
    SetAccount,
    /// Inside a macdef block — skip until blank line or EOF.
    SetMacdef,
}

// ---------------------------------------------------------------------------
// NetrcParser — the main parser and lookup structure
// ---------------------------------------------------------------------------

/// Netrc file parser for `~/.netrc` credentials.
///
/// Parses the netrc content using a state-machine approach identical to the
/// C++ `Netrc::parse()`. Stores entries in order and provides lookup methods
/// that fall back to the `default` entry.
#[derive(Debug, Clone)]
pub struct NetrcParser {
    /// Machine-specific entries, in file order.
    entries: Vec<NetrcEntry>,
    /// The optional `default` entry (matches any host).
    default_entry: Option<NetrcEntry>,
}

impl NetrcParser {
    // -- Construction -------------------------------------------------------

    /// Create an empty parser with no entries.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            default_entry: None,
        }
    }

    // -- Parsing ------------------------------------------------------------

    /// Parse netrc content from a string.
    ///
    /// This is the core parser, ported from C++ `Netrc::parse()`. It uses a
    /// state machine that walks through whitespace-separated tokens, handling
    /// `machine`, `default`, `login`, `password`, `account`, and `macdef`.
    ///
    /// Comments (lines starting with `#`) are skipped. The `default` entry
    /// must come last per the netrc specification.
    ///
    /// # Errors
    ///
    /// Returns [`NetrcError::ParseError`] if:
    /// - A token value is expected but EOF is reached
    /// - A non-keyword token appears where `machine` or `default` is expected
    pub fn parse(input: &str) -> Result<Self, NetrcError> {
        let mut parser = Self::new();
        parser.parse_inner(input)?;
        Ok(parser)
    }

    /// Parse from a file path.
    ///
    /// Reads the file, then delegates to [`Self::parse`].
    pub fn parse_file(path: &Path) -> Result<Self, NetrcError> {
        if !path.exists() {
            return Err(NetrcError::FileNotFound(
                path.to_string_lossy().to_string(),
            ));
        }
        let content = std::fs::read_to_string(path).map_err(|e| {
            NetrcError::IoError(format!("{}: {}", path.display(), e))
        })?;
        Self::parse(&content)
    }

    /// Internal state-machine parser, matching C++ `Netrc::parse()`.
    fn parse_inner(&mut self, input: &str) -> Result<(), NetrcError> {
        let mut current: Option<NetrcEntry> = None;
        let mut state = ParseState::GetToken;

        for line in input.lines() {
            // Skip comment lines
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                // If we are inside macdef, a blank line ends it
                if state == ParseState::SetMacdef && trimmed.is_empty() {
                    state = ParseState::GetToken;
                }
                continue;
            }

            // Inside macdef: skip all non-blank lines
            if state == ParseState::SetMacdef {
                continue;
            }

            // Split into whitespace-separated tokens
            let tokens: Vec<&str> = line.split_whitespace().collect();
            for token in tokens {
                let token_lower = token.to_lowercase();

                match state {
                    ParseState::GetToken => {
                        if token_lower == "machine" {
                            // Store any previous entry
                            self.store_entry(&mut current);
                            current = Some(NetrcEntry::new(String::new()));
                            state = ParseState::SetMachine;
                        } else if token_lower == "default" {
                            self.store_entry(&mut current);
                            current = Some(NetrcEntry::new_default());
                            // default does not consume a following value token
                            state = ParseState::GetToken;
                        } else if let Some(ref mut _entry) = current {
                            // We have an active entry — interpret property keywords
                            if token_lower == "login" {
                                state = ParseState::SetLogin;
                            } else if token_lower == "password" || token_lower == "passwd" {
                                state = ParseState::SetPassword;
                            } else if token_lower == "account" {
                                state = ParseState::SetAccount;
                            } else if token_lower == "macdef" {
                                state = ParseState::SetMacdef;
                                // Skip the rest of the macdef name and body
                                break;
                            }
                            // Unknown tokens in GetToken state are silently ignored,
                            // matching C++ behavior.
                        } else {
                            // No active entry — C++ throws an error here
                            return Err(NetrcError::ParseError(format!(
                                "'{}' encountered where 'machine' or 'default' expected",
                                token
                            )));
                        }
                    }
                    ParseState::SetMachine => {
                        if let Some(ref mut entry) = current {
                            entry.machine = token.to_string();
                        }
                        state = ParseState::GetToken;
                    }
                    ParseState::SetLogin => {
                        if let Some(ref mut entry) = current {
                            entry.login = Some(token.to_string());
                        }
                        state = ParseState::GetToken;
                    }
                    ParseState::SetPassword => {
                        if let Some(ref mut entry) = current {
                            entry.password = Some(token.to_string());
                        }
                        state = ParseState::GetToken;
                    }
                    ParseState::SetAccount => {
                        if let Some(ref mut entry) = current {
                            entry.account = Some(token.to_string());
                        }
                        state = ParseState::GetToken;
                    }
                    ParseState::SetMacdef => {
                        // We already broke out of the token loop for macdef;
                        // this branch is unreachable but kept for exhaustiveness.
                        break;
                    }
                }
            }
        }

        // Check for premature EOF
        if state != ParseState::GetToken && state != ParseState::SetMacdef {
            return Err(NetrcError::ParseError(
                "EOF reached where a token value was expected".to_string(),
            ));
        }

        // Store the last entry
        self.store_entry(&mut current);
        Ok(())
    }

    /// Store a completed entry into the appropriate field.
    ///
    /// Default entries go to `default_entry`; machine entries go to `entries`.
    fn store_entry(&mut self, current: &mut Option<NetrcEntry>) {
        if let Some(entry) = current.take() {
            if entry.is_default() {
                debug!("Netrc: stored default entry");
                self.default_entry = Some(entry);
            } else {
                debug!("Netrc: stored entry for machine={}", entry.machine);
                self.entries.push(entry);
            }
        }
    }

    // -- Lookup -------------------------------------------------------------

    /// Find credentials for a specific machine hostname.
    ///
    /// Returns the first entry whose machine name exactly matches `machine`.
    /// Does not fall back to the default entry.
    pub fn find(&self, machine: &str) -> Option<&NetrcEntry> {
        self.entries.iter().find(|e| e.machine == machine)
    }

    /// Get the default entry, if any.
    pub fn find_default(&self) -> Option<&NetrcEntry> {
        self.default_entry.as_ref()
    }

    /// Find credentials for a machine, falling back to the default entry.
    ///
    /// This matches the C++ `Netrc::findAuthenticator()` logic: first search
    /// machine-specific entries, then return the default entry if no match.
    pub fn find_with_fallback(&self, machine: &str) -> Option<&NetrcEntry> {
        self.find(machine).or_else(|| self.find_default())
    }

    // -- Accessors ----------------------------------------------------------

    /// All machine-specific entries, in file order.
    pub fn entries(&self) -> &[NetrcEntry] {
        &self.entries
    }

    /// The default entry, if present.
    pub fn default_entry(&self) -> Option<&NetrcEntry> {
        self.default_entry.as_ref()
    }

    /// Total number of entries (machine entries + optional default).
    pub fn len(&self) -> usize {
        self.entries.len() + if self.default_entry.is_some() { 1 } else { 0 }
    }

    /// Whether there are no entries at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.default_entry.is_none()
    }
}

impl Default for NetrcParser {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Convenience: locate the default .netrc file
// ---------------------------------------------------------------------------

/// Locate the user's `.netrc` file by checking standard locations.
///
/// On Unix: `$HOME/.netrc`
/// On Windows: `%USERPROFILE%\_netrc` or `%HOMEDRIVE%%HOMEPATH%\_netrc`
///
/// Also checks `.netrc.txt` as some tools use that extension.
pub fn find_netrc_file() -> Option<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            std::env::var_os("HOMEDRIVE").and_then(|d| {
                std::env::var_os("HOMEPATH").map(|p| {
                    let mut s = d.to_os_string();
                    s.push(p);
                    s
                })
            })
        });
    home.and_then(|h| {
        let h = h.to_string_lossy().to_string();
        for name in &[".netrc", "_netrc", ".netrc.txt"] {
            let candidate = format!("{}/{}", h, name);
            if Path::new(&candidate).exists() {
                return Some(candidate);
            }
        }
        None
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_machine() {
        let content = "machine ftp.example.com\nlogin myuser\npassword mypass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.len(), 1);
        assert!(!parser.is_empty());

        let entry = parser.find("ftp.example.com").unwrap();
        assert_eq!(entry.machine, "ftp.example.com");
        assert_eq!(entry.login.as_deref(), Some("myuser"));
        assert_eq!(entry.password.as_deref(), Some("mypass"));
        assert!(entry.account.is_none());
    }

    #[test]
    fn test_parse_with_account() {
        let content = "machine host.example.com\nlogin user1\npassword pass1\naccount acct123\n";
        let parser = NetrcParser::parse(content).unwrap();
        let entry = parser.find("host.example.com").unwrap();
        assert_eq!(entry.account.as_deref(), Some("acct123"));
    }

    #[test]
    fn test_parse_default_entry() {
        let content = "default\nlogin anonymous\npassword guest@\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert!(parser.find("anything.example.com").is_none());
        let def = parser.find_default().unwrap();
        assert!(def.is_default());
        assert_eq!(def.login.as_deref(), Some("anonymous"));
        assert_eq!(def.password.as_deref(), Some("guest@"));
    }

    #[test]
    fn test_parse_machine_and_default() {
        let content = "\
            machine ftp.example.com\n\
            login myuser\n\
            password mypass\n\
            default\n\
            login anon\n\
            password guest@\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.len(), 2);

        // Exact match
        let entry = parser.find("ftp.example.com").unwrap();
        assert_eq!(entry.login.as_deref(), Some("myuser"));

        // Fallback to default
        let fallback = parser.find_with_fallback("unknown.host.com").unwrap();
        assert!(fallback.is_default());
        assert_eq!(fallback.login.as_deref(), Some("anon"));
    }

    #[test]
    fn test_parse_multiple_machines() {
        let content = "\
            machine ftp.example.com\n\
            login user1\n\
            password pass1\n\
            \n\
            machine ssh.example.com\n\
            login user2\n\
            password pass2\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.entries().len(), 2);
        assert_eq!(parser.find("ftp.example.com").unwrap().login.as_deref(), Some("user1"));
        assert_eq!(parser.find("ssh.example.com").unwrap().login.as_deref(), Some("user2"));
    }

    #[test]
    fn test_find_unknown_returns_none() {
        let content = "machine ftp.example.com\nlogin user\npassword pass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert!(parser.find("unknown.com").is_none());
        assert!(parser.find_with_fallback("unknown.com").is_none());
    }

    #[test]
    fn test_comments_and_blank_lines() {
        let content = "\
            # This is a comment\n\
            machine example.com\n\
            # Another comment\n\
            login user\n\
               password pass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.len(), 1);
        let entry = &parser.entries()[0];
        assert_eq!(entry.login.as_deref(), Some("user"));
        assert_eq!(entry.password.as_deref(), Some("pass"));
    }

    #[test]
    fn test_case_insensitive_keywords() {
        let content = "MACHINE example.com\nLOGIN user\nPASSWORD pass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.entries()[0].login.as_deref(), Some("user"));
    }

    #[test]
    fn test_passwd_alias() {
        let content = "machine example.com\nlogin user\npasswd secret\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.entries()[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn test_inline_tokens() {
        // Tokens on a single line, matching C++ splitIterM behavior
        let content = "machine example.com login user password pass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.len(), 1);
        let entry = &parser.entries()[0];
        assert_eq!(entry.machine, "example.com");
        assert_eq!(entry.login.as_deref(), Some("user"));
        assert_eq!(entry.password.as_deref(), Some("pass"));
    }

    #[test]
    fn test_macdef_skipped() {
        let content = "\
            machine example.com\n\
            login user\n\
            password pass\n\
            macdef mymacro\n\
            this is macro body\n\
            \n\
            machine other.com\n\
            login otheruser\n\
            password otherpass\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.entries().len(), 2);
        assert_eq!(parser.find("example.com").unwrap().login.as_deref(), Some("user"));
        assert_eq!(parser.find("other.com").unwrap().login.as_deref(), Some("otheruser"));
    }

    #[test]
    fn test_error_token_before_machine() {
        let content = "login user\n";
        let result = NetrcParser::parse(content);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            NetrcError::ParseError(msg) => {
                assert!(msg.contains("'login'"));
                assert!(msg.contains("machine") || msg.contains("default"));
            }
            _ => panic!("expected ParseError, got {:?}", err),
        }
    }

    #[test]
    fn test_error_premature_eof() {
        // "machine" keyword without a hostname value
        let content = "machine\n";
        let result = NetrcParser::parse(content);
        assert!(result.is_err());
        match result.unwrap_err() {
            NetrcError::ParseError(msg) => {
                assert!(msg.contains("EOF") || msg.contains("token"));
            }
            _ => panic!("expected ParseError"),
        }
    }

    #[test]
    fn test_error_login_without_value() {
        let content = "machine example.com\nlogin\n";
        let result = NetrcParser::parse(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_input() {
        let parser = NetrcParser::parse("").unwrap();
        assert!(parser.is_empty());
        assert_eq!(parser.len(), 0);
    }

    #[test]
    fn test_only_comments() {
        let content = "# just a comment\n# another comment\n";
        let parser = NetrcParser::parse(content).unwrap();
        assert!(parser.is_empty());
    }

    #[test]
    fn test_entry_has_credentials() {
        let mut entry = NetrcEntry::new("host".to_string());
        assert!(!entry.has_credentials());
        entry.login = Some("user".to_string());
        assert!(!entry.has_credentials());
        entry.password = Some("pass".to_string());
        assert!(entry.has_credentials());
    }

    #[test]
    fn test_entry_is_default() {
        let machine_entry = NetrcEntry::new("host".to_string());
        assert!(!machine_entry.is_default());

        let default_entry = NetrcEntry::new_default();
        assert!(default_entry.is_default());
    }

    #[test]
    fn test_error_display() {
        let err = NetrcError::FileNotFound("/missing/.netrc".into());
        assert!(err.to_string().contains("/missing/.netrc"));

        let err2 = NetrcError::ParseError("bad token".into());
        assert!(err2.to_string().contains("parse error"));

        let err3 = NetrcError::IoError("read failed".into());
        assert!(err3.to_string().contains("I/O"));
    }

    #[test]
    fn test_parse_file_not_found() {
        let result = NetrcParser::parse_file(Path::new("/nonexistent/.netrc"));
        assert!(result.is_err());
        match result.unwrap_err() {
            NetrcError::FileNotFound(_) => {}
            other => panic!("expected FileNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_to_netrc_store() {
        let content = "\
            machine ftp.example.com\n\
            login myuser\n\
            password mypass\n\
            default\n\
            login anon\n\
            password guest@\n";
        let parser = NetrcParser::parse(content).unwrap();
        let store: super::super::NetrcStore = parser.into();

        // Machine entry
        let entry = store.find("ftp.example.com").unwrap();
        assert_eq!(entry.login, "myuser");
        assert_eq!(entry.password, "mypass");

        // Default entry accessible via default_entry()
        let default = store.default_entry();
        assert!(default.is_some());
        assert_eq!(default.unwrap().login, "anon");

        // Fallback lookup
        let fallback = store.find_with_fallback("unknown.host.com").unwrap();
        assert_eq!(fallback.login, "anon");
    }

    #[test]
    fn test_convert_skips_entries_without_credentials() {
        // Entry with only login, no password — should be skipped
        let content = "machine incomplete.com\nlogin user\n";
        let parser = NetrcParser::parse(content).unwrap();
        let store: super::super::NetrcStore = parser.into();
        assert!(store.is_empty());
    }

    #[test]
    fn test_real_world_netrc_format() {
        let content = "\
            # ~/.netrc\n\
            machine ftp.example.com\n\
            login myuser\n\
            password mypass\n\
            \n\
            machine api.example.com\n\
            login apiuser\n\
            password apipass\n\
            account prod\n\
            \n\
            default\n\
            login anonymous\n\
            password guest@\n";
        let parser = NetrcParser::parse(content).unwrap();

        assert_eq!(parser.entries().len(), 2);
        assert!(parser.default_entry().is_some());

        // Exact lookup
        let ftp = parser.find("ftp.example.com").unwrap();
        assert_eq!(ftp.login.as_deref(), Some("myuser"));
        assert!(ftp.account.is_none());

        let api = parser.find("api.example.com").unwrap();
        assert_eq!(api.login.as_deref(), Some("apiuser"));
        assert_eq!(api.account.as_deref(), Some("prod"));

        // Fallback
        let fallback = parser.find_with_fallback("unknown.com").unwrap();
        assert_eq!(fallback.login.as_deref(), Some("anonymous"));
    }

    #[test]
    fn test_tabs_as_whitespace() {
        let content = "machine\texample.com\tlogin\tuser\tpassword\tpass\n";
        let parser = NetrcParser::parse(content).unwrap();
        let entry = parser.find("example.com").unwrap();
        assert_eq!(entry.login.as_deref(), Some("user"));
        assert_eq!(entry.password.as_deref(), Some("pass"));
    }

    #[test]
    fn test_mixed_whitespace_and_comments() {
        let content = "  machine   example.com   # inline comment\n  login   user\n  password   pass\n";
        // Note: inline comments after tokens are NOT standard netrc.
        // The C++ parser doesn't support them either — the "# inline" would
        // be parsed as extra tokens. Here we just verify the parser doesn't
        // crash and picks up the first tokens correctly.
        let parser = NetrcParser::parse(content).unwrap();
        assert_eq!(parser.entries()[0].machine, "example.com");
    }
}
