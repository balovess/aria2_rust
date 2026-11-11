use std::path::PathBuf;

use clap::Args;

// =========================================================================
// HTTP/FTP Options
// =========================================================================

/// HTTP/FTP options: proxies, headers, timeouts, connection management.
#[derive(Args, Debug)]
#[command(next_help_heading = "HTTP/FTP options")]
pub struct HttpFtpArgs {
    /// Global proxy URL
    #[arg(long = "all-proxy")]
    pub all_proxy: Option<String>,

    /// HTTP proxy URL
    #[arg(long = "http-proxy")]
    pub http_proxy: Option<String>,

    /// HTTPS proxy URL
    #[arg(long = "https-proxy")]
    pub https_proxy: Option<String>,

    /// FTP proxy URL
    #[arg(long = "ftp-proxy")]
    pub ftp_proxy: Option<String>,

    /// All proxy username
    #[arg(long = "all-proxy-user")]
    pub all_proxy_user: Option<String>,

    /// All proxy password
    #[arg(long = "all-proxy-passwd")]
    pub all_proxy_passwd: Option<String>,

    /// HTTP proxy username
    #[arg(long = "http-proxy-user")]
    pub http_proxy_user: Option<String>,

    /// HTTP proxy password
    #[arg(long = "http-proxy-passwd")]
    pub http_proxy_passwd: Option<String>,

    /// HTTPS proxy username
    #[arg(long = "https-proxy-user")]
    pub https_proxy_user: Option<String>,

    /// HTTPS proxy password
    #[arg(long = "https-proxy-passwd")]
    pub https_proxy_passwd: Option<String>,

    /// FTP proxy username
    #[arg(long = "ftp-proxy-user")]
    pub ftp_proxy_user: Option<String>,

    /// FTP proxy password
    #[arg(long = "ftp-proxy-passwd")]
    pub ftp_proxy_passwd: Option<String>,

    /// Proxy method (get/tunnel)
    #[arg(long = "proxy-method")]
    pub proxy_method: Option<String>,

    /// Proxy exclusion list (comma-separated domains)
    #[arg(long = "no-proxy")]
    pub no_proxy: Option<String>,

    /// User-Agent header
    #[arg(short = 'U', long = "user-agent")]
    pub user_agent: Option<String>,

    /// Referer header
    #[arg(long)]
    pub referer: Option<String>,

    /// Custom headers (Header:Value pairs, can be repeated)
    #[arg(long)]
    pub header: Vec<String>,

    /// Cookie file to load
    #[arg(long = "load-cookies")]
    pub load_cookies: Option<PathBuf>,

    /// Cookie file to save
    #[arg(long = "save-cookies")]
    pub save_cookies: Option<PathBuf>,

    /// Connect timeout in seconds
    #[arg(long = "connect-timeout")]
    pub connect_timeout: Option<u64>,

    /// I/O timeout in seconds
    #[arg(short = 't', long)]
    pub timeout: Option<u64>,

    /// Max retry attempts
    #[arg(short = 'm', long = "max-tries")]
    pub max_tries: Option<u64>,

    /// Retry wait time in seconds
    #[arg(long = "retry-wait")]
    pub retry_wait: Option<u64>,

    /// Per-download budget for simultaneous HTTP Range requests; actual concurrency adapts below it
    #[arg(short = 's', long)]
    pub split: Option<u64>,

    /// Min split size (e.g. 1M, 20M)
    #[arg(short = 'k', long = "min-split-size")]
    pub min_split_size: Option<String>,

    /// Minimum adaptive HTTP Range size after explicit server rejection (default: 64K)
    #[arg(long = "min-http-range-size")]
    pub min_http_range_size: Option<String>,

    /// Hard per-download, per-server TCP connection ceiling; HTTP/2 Range streams may share a connection
    #[arg(short = 'x', long = "max-connection-per-server")]
    pub max_connection_per_server: Option<u64>,

    /// Maximum independent HTTP/2 client pools per server and download; capped by -x (default: 4)
    #[arg(long = "max-http2-sessions-per-server")]
    pub max_http2_sessions_per_server: Option<u64>,

    /// Fixed HTTP/2 Range streams per physical session (default: 4)
    #[arg(long = "max-http2-streams-per-session")]
    pub max_http2_streams_per_session: Option<u64>,

    /// Enable adaptive HTTP Range concurrency target adjustments
    #[arg(
        long = "http-adaptive-enabled",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_adaptive_enabled: Option<bool>,

    /// Reduce Range concurrency by this percentage after server capacity rejection
    #[arg(long = "http-adaptive-reduction-percent")]
    pub http_adaptive_reduction_percent: Option<u64>,

    /// Minimum seconds between upward concurrency probes
    #[arg(long = "http-adaptive-recovery-interval")]
    pub http_adaptive_recovery_interval: Option<u64>,

    /// Minimum successful Ranges before an upward concurrency probe
    #[arg(long = "http-adaptive-recovery-successes-min")]
    pub http_adaptive_recovery_successes_min: Option<u64>,

    /// Maximum successful Ranges before an upward concurrency probe
    #[arg(long = "http-adaptive-recovery-successes-max")]
    pub http_adaptive_recovery_successes_max: Option<u64>,

    /// Initial delay between starting HTTP Range requests, in milliseconds
    #[arg(long = "http-adaptive-start-interval-ms")]
    pub http_adaptive_start_interval_ms: Option<u64>,

    /// Base capacity-rejection backoff, in seconds
    #[arg(long = "http-adaptive-backoff-base")]
    pub http_adaptive_backoff_base: Option<u64>,

    /// Maximum capacity-rejection backoff, in seconds
    #[arg(long = "http-adaptive-backoff-max")]
    pub http_adaptive_backoff_max: Option<u64>,

    /// Initial delay between Range starts after capacity rejection, in milliseconds
    #[arg(long = "http-adaptive-capacity-start-interval-ms")]
    pub http_adaptive_capacity_start_interval_ms: Option<u64>,

    /// Maximum paced-start delay after repeated capacity rejection, in milliseconds
    #[arg(long = "http-adaptive-capacity-start-interval-max-ms")]
    pub http_adaptive_capacity_start_interval_max_ms: Option<u64>,

    /// Successful Ranges required before easing request pacing
    #[arg(long = "http-adaptive-rate-recovery-successes")]
    pub http_adaptive_rate_recovery_successes: Option<u64>,

    /// Minimum delay between Range starts after recovery, in milliseconds
    #[arg(long = "http-adaptive-rate-min-interval-ms")]
    pub http_adaptive_rate_min_interval_ms: Option<u64>,

    /// Quiet seconds without capacity rejection before resetting backoff
    #[arg(long = "http-adaptive-backoff-reset-after")]
    pub http_adaptive_backoff_reset_after: Option<u64>,

    /// HTTP version: prefer HTTP/2 with HTTP/1.1 fallback, or force one version
    #[arg(long = "http-version", value_parser = ["auto", "1.1", "2"])]
    pub http_version: Option<String>,

    /// Max pipelined HTTP requests per connection
    #[arg(long = "max-http-pipelining", hide = true)]
    pub max_http_pipelining: Option<u64>,

    /// Verify SSL certificate
    #[arg(
        long = "check-certificate",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub check_certificate: Option<bool>,

    /// Disable SSL certificate verification
    #[arg(
        long = "no-check-certificate",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_check_certificate: Option<bool>,

    /// CA certificate file
    #[arg(long = "ca-certificate")]
    pub ca_certificate: Option<PathBuf>,

    /// Client certificate file path (PEM format)
    #[arg(long = "certificate")]
    pub certificate: Option<PathBuf>,

    /// Client private key file path (PEM format)
    #[arg(long = "private-key")]
    pub private_key: Option<PathBuf>,

    /// Minimum TLS version (TLSv1.1/TLSv1.2/TLSv1.3)
    #[arg(long = "min-tls-version")]
    pub min_tls_version: Option<String>,

    /// Allow overwriting existing files
    #[arg(
        long = "allow-overwrite",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub allow_overwrite: Option<bool>,

    /// Auto rename conflicting files
    #[arg(
        long = "auto-file-renaming",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub auto_file_renaming: Option<bool>,

    /// Resume partial downloads
    #[arg(
        short = 'c',
        long = "continue",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub continue_dl: Option<bool>,

    /// Disable resume of partial downloads
    #[arg(
        long = "no-continue",
        hide = true,
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_continue: Option<bool>,

    /// Use remote file timestamp
    #[arg(
        short = 'R',
        long = "remote-time",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub remote_time: Option<bool>,

    /// Enable HTTP persistent connection (keep-alive)
    #[arg(
        long = "enable-http-keep-alive",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_http_keep_alive: Option<bool>,

    /// Enable HTTP/1.1 pipelining
    #[arg(
        long = "enable-http-pipelining",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_http_pipelining: Option<bool>,

    /// Accept gzip-encoded HTTP responses
    #[arg(
        long = "http-accept-gzip",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_accept_gzip: Option<bool>,

    /// Send HTTP authentication header only after challenge
    #[arg(
        long = "http-auth-challenge",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_auth_challenge: Option<bool>,

    /// Send Cache-Control: no-cache with requests
    #[arg(
        long = "http-no-cache",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub http_no_cache: Option<bool>,

    /// Treat Content-Disposition filename as UTF-8
    #[arg(
        long = "content-disposition-default-utf8",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub content_disposition_default_utf8: Option<bool>,

    /// Use HEAD method for file existence checks
    #[arg(
        long = "use-head",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub use_head: Option<bool>,

    /// Omit Want-Digest header from HTTP requests
    #[arg(
        long = "no-want-digest-header",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub no_want_digest_header: Option<bool>,

    /// HTTP authentication username
    #[arg(long = "http-user")]
    pub http_user: Option<String>,

    /// HTTP authentication password
    #[arg(long = "http-passwd")]
    pub http_passwd: Option<String>,

    /// FTP authentication username
    #[arg(long = "ftp-user")]
    pub ftp_user: Option<String>,

    /// FTP authentication password
    #[arg(long = "ftp-passwd")]
    pub ftp_passwd: Option<String>,

    /// Use FTP passive mode
    #[arg(
        short = 'p',
        long = "ftp-pasv",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub ftp_pasv: Option<bool>,

    /// Reuse FTP data connection across downloads
    #[arg(
        long = "ftp-reuse-connection",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub ftp_reuse_connection: Option<bool>,

    /// FTP transfer type (binary/ascii)
    #[arg(long = "ftp-type")]
    pub ftp_type: Option<String>,

    /// SSH host key fingerprint (hashType=digest format)
    #[arg(long = "ssh-host-key-md")]
    pub ssh_host_key_md: Option<String>,

    /// SFTP SSH connection timeout in seconds
    #[arg(long = "sftp-connect-timeout")]
    pub sftp_connect_timeout: Option<u64>,

    /// SFTP I/O read timeout in seconds
    #[arg(long = "sftp-read-timeout")]
    pub sftp_read_timeout: Option<u64>,

    /// Enable SSH compression for SFTP
    #[arg(long = "sftp-compression", num_args(0..=1), require_equals = true, default_missing_value = "true")]
    pub sftp_compression: Option<bool>,

    /// SSH keepalive interval in seconds for SFTP; 0 disables keepalives
    #[arg(long = "sftp-keepalive-interval")]
    pub sftp_keepalive_interval: Option<u64>,

    /// Comma-separated preferred SSH ciphers for SFTP
    #[arg(long = "sftp-preferred-ciphers")]
    pub sftp_preferred_ciphers: Option<String>,
}
