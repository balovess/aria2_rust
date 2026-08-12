//! Netrc file parser: `NetrcParser` struct and its state-machine implementation.

use std::path::Path;

use tracing::debug;

use super::error::NetrcError;
use super::no_proxy_domain_match;
use super::types::{NetrcEntry, ParseState};

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
            return Err(NetrcError::FileNotFound(path.to_string_lossy().to_string()));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| NetrcError::IoError(format!("{}: {}", path.display(), e)))?;
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
                        } else if current.is_some() {
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
    /// Returns the first entry whose machine name matches `machine`.
    ///
    /// A machine name beginning with `.` matches a subdomain suffix, following
    /// aria2's `noProxyDomainMatch` semantics.
    /// Does not fall back to the default entry.
    pub fn find(&self, machine: &str) -> Option<&NetrcEntry> {
        self.entries
            .iter()
            .find(|e| no_proxy_domain_match(machine, &e.machine))
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
