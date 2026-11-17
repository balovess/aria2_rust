use std::fmt;

/// A resolved user:password pair ready for inclusion in an Authorization header.
///
/// Call [`AuthConfig::auth_text`] to get the `user:password` string, or pass it
/// to the HTTP authentication helpers to build a header value.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    user: String,
    password: String,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("user", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

impl AuthConfig {
    /// Create a new `AuthConfig`. Returns `None` if `user` is empty.
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

    /// Returns `user:password` for HTTP authentication construction.
    pub fn auth_text(&self) -> String {
        format!("{}:{}", self.user, self.password)
    }
}

impl fmt::Display for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.user, self.password)
    }
}

/// A cached Basic-auth credential scoped to a host, port, and path prefix.
///
/// The path is normalized to end with `/` so prefix matching respects path
/// segments.
#[derive(Clone)]
pub struct BasicCred {
    /// Username.
    pub user: String,
    /// Password.
    pub password: String,
    /// Hostname this credential applies to.
    pub host: String,
    /// Port this credential applies to.
    pub port: u16,
    /// Path prefix, always ending with `/`.
    pub path: String,
    /// Whether this credential has been activated by a 401 challenge.
    pub activated: bool,
}

impl fmt::Debug for BasicCred {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BasicCred")
            .field("user", &"<redacted>")
            .field("password", &"<redacted>")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("activated", &self.activated)
            .finish()
    }
}

impl BasicCred {
    /// Create a `BasicCred`, normalizing the path to end with `/`.
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

    /// Activate this credential after an authentication challenge.
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
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.host.cmp(&other.host) {
            std::cmp::Ordering::Equal => match self.port.cmp(&other.port) {
                std::cmp::Ordering::Equal => other.path.cmp(&self.path),
                order => order,
            },
            order => order,
        }
    }
}
