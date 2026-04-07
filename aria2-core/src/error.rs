use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum Aria2Error {
    #[error("网络错误: {0}")]
    Network(String),

    #[error("IO错误: {0}")]
    Io(String),

    #[error("解析错误: {0}")]
    Parse(String),

    #[error("校验失败: {0}")]
    Checksum(String),

    #[error("下载失败: {0}")]
    DownloadFailed(String),

    #[error("可恢复错误: {0}")]
    Recoverable(#[from] RecoverableError),

    #[error("致命错误: {0}")]
    Fatal(#[from] FatalError),
}

impl From<serde_json::Error> for Aria2Error {
    fn from(err: serde_json::Error) -> Self {
        Aria2Error::Parse(err.to_string())
    }
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum RecoverableError {
    #[error("连接超时")]
    Timeout,

    #[error("服务器返回错误: {code}")]
    ServerError { code: u16 },

    #[error("临时网络故障: {message}")]
    TemporaryNetworkFailure { message: String },

    #[error("重试次数已用尽: {attempts}次")]
    MaxTriesReached { attempts: u32 },
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum FatalError {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("磁盘空间不足")]
    DiskSpaceExhausted,

    #[error("权限被拒绝: {path}")]
    PermissionDenied { path: String },

    #[error("文件不存在: {path}")]
    FileNotFound { path: String },

    #[error("不支持的协议: {protocol}")]
    UnsupportedProtocol { protocol: String },
}

pub type Result<T> = std::result::Result<T, Aria2Error>;
