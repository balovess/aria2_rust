use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq)]
pub enum Aria2Error {
    #[error("Network error: {0}")]
    Network(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("JSON parse error: {0}")]
    JsonParse(String),

    #[error("Metalink parse error: {0}")]
    MetalinkParse(String),

    #[error("Bencode parse error: {0}")]
    BencodeParse(String),

    #[error("BitTorrent parse error: {0}")]
    BittorrentParse(String),

    #[error("Magnet parse error: {0}")]
    MagnetParse(String),

    #[error("Checksum failed: {0}")]
    Checksum(String),

    #[error("HTTP protocol error: {0}")]
    HttpProtocol(String),

    #[error("FTP protocol error: {0}")]
    FtpProtocol(String),

    #[error("File open error: {0}")]
    FileOpen(String),

    #[error("File create error: {0}")]
    FileCreate(String),

    #[error("File I/O error: {0}")]
    FileIo(String),

    #[error("File already exists: {0}")]
    FileAlreadyExists(String),

    #[error("File renaming failed: {0}")]
    FileRenamingFailed(String),

    #[error("Directory create error: {0}")]
    DirCreate(String),

    #[error("Name resolve error: {0}")]
    NameResolve(String),

    #[error("Download failed: {0}")]
    DownloadFailed(String),

    #[error("Recoverable error: {0}")]
    Recoverable(#[from] RecoverableError),

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Fatal error: {0}")]
    Fatal(#[from] FatalError),
}

impl From<serde_json::Error> for Aria2Error {
    fn from(err: serde_json::Error) -> Self {
        Aria2Error::JsonParse(err.to_string())
    }
}

impl From<std::io::Error> for Aria2Error {
    fn from(err: std::io::Error) -> Self {
        Aria2Error::Io(err.to_string())
    }
}

impl From<std::string::FromUtf8Error> for Aria2Error {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Aria2Error::Parse(err.to_string())
    }
}

impl From<base64::DecodeError> for Aria2Error {
    fn from(err: base64::DecodeError) -> Self {
        Aria2Error::Parse(err.to_string())
    }
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum RecoverableError {
    #[error("Connection timeout")]
    Timeout,

    #[error("Server returned error: {code}")]
    ServerError { code: u16 },

    #[error("Range not satisfiable: {range}")]
    RangeNotSatisfiable { range: String },

    #[error("Temporary network failure: {message}")]
    TemporaryNetworkFailure { message: String },

    #[error("Max retries reached: {attempts} attempts")]
    MaxTriesReached { attempts: u32 },

    #[error("Invalid piece index: {index} (max: {max_index})")]
    InvalidPieceIndex { index: u32, max_index: u32 },

    #[error("FTP server does not support resuming (CANNOT_RESUME)")]
    CannotResume,

    #[error("FTP protocol error: {message}")]
    FtpProtocolError { message: String },

    #[error("HTTP protocol error: {message}")]
    HttpProtocolError { message: String },

    #[error("Resource not found")]
    ResourceNotFound,

    #[error("Reached max-file-not-found limit")]
    MaxFileNotFound,

    #[error("HTTP authentication failed: {message}")]
    HttpAuthFailed { message: String },

    #[error("Too many redirects: count={count}")]
    HttpTooManyRedirects { count: u32 },

    #[error("Handshake rejected: {reason}")]
    HandshakeRejection { reason: String },
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum FatalError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Insufficient disk space")]
    DiskSpaceExhausted,

    #[error("Permission denied: {path}")]
    PermissionDenied { path: String },

    #[error("File not found: {path}")]
    FileNotFound { path: String },

    #[error("Unsupported protocol: {protocol}")]
    UnsupportedProtocol { protocol: String },
}

pub type Result<T> = std::result::Result<T, Aria2Error>;
