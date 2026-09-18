use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::time::Duration;

/// FTP connection configuration options.
#[derive(Debug, Clone)]
pub struct FtpOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub passive_mode: bool,
    pub username: String,
    pub password: String,
    /// Keep-alive interval for control channel (None to disable).
    pub keepalive_interval: Option<Duration>,
    /// Maximum number of retry attempts for transient errors.
    pub max_retries: u32,
}

impl Default for FtpOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(30),
            passive_mode: true,
            username: "anonymous".to_string(),
            password: "aria2@".to_string(),
            keepalive_interval: Some(Duration::from_secs(60)),
            max_retries: 3,
        }
    }
}

/// FTP response code classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FtpResponseClass {
    /// Positive Preliminary (1xx): Command accepted, waiting for confirmation.
    PositivePreliminary,
    /// Positive Completion (2xx): Command completed successfully.
    PositiveCompletion,
    /// Positive Intermediate (3xx): Command accepted, additional info needed.
    PositiveIntermediate,
    /// Transient Negative (4xx): Temporary failure, retry may succeed.
    TransientNegative,
    /// Permanent Negative (5xx): Permanent failure, do not retry.
    PermanentNegative,
    /// Unknown/Invalid response code.
    Unknown,
}

impl FtpResponseClass {
    /// Classify an FTP response code into its category.
    pub fn from_code(code: u16) -> Self {
        match code {
            100..=199 => Self::PositivePreliminary,
            200..=299 => Self::PositiveCompletion,
            300..=399 => Self::PositiveIntermediate,
            400..=499 => Self::TransientNegative,
            500..=599 => Self::PermanentNegative,
            _ => Self::Unknown,
        }
    }

    /// Check if this response class indicates success (1xx-3xx).
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            Self::PositivePreliminary | Self::PositiveCompletion | Self::PositiveIntermediate
        )
    }

    /// Check if this is a retry-worthy transient error (4xx).
    pub fn is_transient(&self) -> bool {
        *self == Self::TransientNegative
    }

    /// Check if this is a permanent failure (5xx).
    pub fn is_permanent(&self) -> bool {
        *self == Self::PermanentNegative
    }
}

/// FTP server response with code and message.
#[derive(Debug, Clone)]
pub struct FtpResponse {
    pub code: u16,
    pub message: String,
}

impl FtpResponse {
    pub fn is_success(&self) -> bool {
        (100..400).contains(&self.code)
    }

    pub fn is_intermediate(&self) -> bool {
        (100..200).contains(&self.code)
    }

    pub fn is_positive_completion(&self) -> bool {
        (200..300).contains(&self.code)
    }

    pub fn is_positive_preliminary(&self) -> bool {
        (100..200).contains(&self.code)
    }

    /// Get the response class for this response.
    pub fn class(&self) -> FtpResponseClass {
        FtpResponseClass::from_code(self.code)
    }

    /// Check if this response indicates a transient error (retry-worthy).
    pub fn is_transient_error(&self) -> bool {
        self.class().is_transient()
    }

    /// Check if this response indicates a permanent error (do not retry).
    pub fn is_permanent_error(&self) -> bool {
        self.class().is_permanent()
    }
}

/// FTP control connection state.
pub struct FtpConnection {
    pub stream: BufReader<TcpStream>,
    pub options: FtpOptions,
    #[allow(dead_code)]
    pub host: String,
    #[allow(dead_code)]
    pub port: u16,
}
