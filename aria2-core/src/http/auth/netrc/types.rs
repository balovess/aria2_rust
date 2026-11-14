//! Core types for netrc parsing: `NetrcEntry` and `ParseState`.

use std::fmt;

// ---------------------------------------------------------------------------
// NetrcEntry — a single .netrc entry for a specific machine
// ---------------------------------------------------------------------------

/// A single Netrc entry for a specific machine.
///
/// Mirrors the C++ `Authenticator` class. Each entry holds credentials
/// scoped to a machine hostname. The `default` entry uses an empty `machine`
/// string and matches any host.
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for NetrcEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetrcEntry")
            .field("machine", &self.machine)
            .field("login", &self.login.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("account", &self.account.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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
// ParseState — internal parser state machine
// ---------------------------------------------------------------------------

/// Internal parser state, matching the C++ `STATE` enum exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseState {
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
