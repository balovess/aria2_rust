//! HTTP/FTP category options: proxies, headers, timeouts, connections, SSL/TLS, file handling.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register HTTP/FTP download options: proxies, headers, timeouts, connection management.
    pub fn register_http_ftp_options(&mut self) {
        // --- Proxy Settings ---
        self.register(OptionDef {
            name: "all-proxy".into(),
            opt_type: OptionType::String,
            description: "Global proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-proxy".into(),
            opt_type: OptionType::String,
            description: "HTTP proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "https-proxy".into(),
            opt_type: OptionType::String,
            description: "HTTPS proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-proxy".into(),
            opt_type: OptionType::String,
            description: "FTP proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-proxy".into(),
            opt_type: OptionType::List,
            description: "Proxy exclusion list (comma-separated domains)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "all-proxy-user".into(),
            opt_type: OptionType::String,
            description: "All proxy username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "all-proxy-passwd".into(),
            opt_type: OptionType::String,
            description: "All proxy password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-proxy-user".into(),
            opt_type: OptionType::String,
            description: "HTTP proxy username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-proxy-passwd".into(),
            opt_type: OptionType::String,
            description: "HTTP proxy password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "https-proxy-user".into(),
            opt_type: OptionType::String,
            description: "HTTPS proxy username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "https-proxy-passwd".into(),
            opt_type: OptionType::String,
            description: "HTTPS proxy password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-proxy-user".into(),
            opt_type: OptionType::String,
            description: "FTP proxy username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-proxy-passwd".into(),
            opt_type: OptionType::String,
            description: "FTP proxy password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "proxy-method".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("get".into()),
            allowed_values: &["get", "tunnel"],
            description: "Proxy method (get/tunnel)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- HTTP Headers & Identity ---
        self.register(OptionDef {
            name: "user-agent".into(),
            opt_type: OptionType::String,
            short_name: Some('U'),
            default_value: OptionValue::Str(aria2_protocol::identity::DEFAULT_USER_AGENT.into()),
            description: "User-Agent header".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "referer".into(),
            opt_type: OptionType::String,
            description: "Referer header".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "header".into(),
            opt_type: OptionType::List,
            cumulative_delimiter: Some("\n"),
            description: "Custom headers (Header:Value pairs)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Cookies ---
        self.register(OptionDef {
            name: "load-cookies".into(),
            opt_type: OptionType::Path,
            description: "Cookie file to load".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-cookies".into(),
            opt_type: OptionType::Path,
            description: "Cookie file to save".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Timeouts & Retries ---
        self.register(OptionDef {
            name: "connect-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(1),
            max: Some(600),
            description: "Connect timeout in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "timeout".into(),
            opt_type: OptionType::Integer,
            short_name: Some('t'),
            default_value: OptionValue::Int(60),
            min: Some(1),
            max: Some(600),
            description: "I/O timeout in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-tries".into(),
            opt_type: OptionType::Integer,
            short_name: Some('m'),
            default_value: OptionValue::Int(5),
            min: Some(0),
            description: "Max retry attempts".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "retry-wait".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(600),
            description: "Retry wait time in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Connection Management ---
        self.register(OptionDef {
            name: "split".into(),
            opt_type: OptionType::Integer,
            short_name: Some('s'),
            default_value: OptionValue::Int(crate::constants::DEFAULT_SPLIT as i64),
            min: Some(1),
            description: "Concurrent segment requests per download".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "min-split-size".into(),
            opt_type: OptionType::Size,
            short_name: Some('k'),
            default_value: OptionValue::Int(crate::constants::DEFAULT_MIN_SPLIT_SIZE as i64),
            min: Some(1024 * 1024),
            max: Some(1024 * 1024 * 1024),
            description: "Min split size".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "min-http-range-size".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(
                crate::constants::DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES as i64,
            ),
            min: Some(crate::constants::HTTP_RANGE_SIZE_FLOOR_MIN_BYTES as i64),
            max: Some(crate::constants::HTTP_RANGE_SIZE_FLOOR_MAX_BYTES),
            description: "Minimum HTTP Range request size after an explicit server size rejection; independent of fixed resume pieces".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-connection-per-server".into(),
            opt_type: OptionType::Integer,
            short_name: Some('x'),
            default_value: OptionValue::Int(
                crate::constants::DEFAULT_MAX_CONNECTION_PER_SERVER as i64,
            ),
            min: Some(1),
            max: Some(crate::constants::MAX_CONNECTION_PER_SERVER as u64),
            description: "Maximum concurrent HTTP/1.1 connections per server for one download; HTTP/2 session and stream limits use their dedicated options".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-http2-sessions-per-server".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                crate::constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as i64,
            ),
            min: Some(1),
            max: Some(crate::constants::MAX_HTTP2_SESSIONS_PER_SERVER as u64),
            description: "Maximum separate HTTP/2 sessions used for concurrent Range traffic to one server; independent of max-connection-per-server".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-http2-streams-per-session".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                crate::constants::DEFAULT_HTTP2_STREAMS_PER_SESSION as i64,
            ),
            min: Some(1),
            max: Some(256),
            description:
                "Fixed maximum concurrent Range streams per HTTP/2 connection; not auto-tuned"
                    .into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-version".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("auto".into()),
            allowed_values: &["auto", "1.1", "2"],
            description: "HTTP protocol selection: auto prefers HTTP/2 over TLS and falls back to HTTP/1.1; 1.1 and 2 force one protocol".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Adaptive HTTP Range concurrency ---
        self.register(OptionDef {
            name: "http-adaptive-enabled".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable adaptive HTTP Range concurrency target adjustments and adaptive capacity cooldown".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        for (name, default, min, max, description) in [
            (
                "http-adaptive-reduction-percent",
                25,
                1,
                90,
                "Concurrency reduction percentage after an explicit capacity rejection",
            ),
            (
                "http-adaptive-recovery-interval",
                10,
                1,
                3600,
                "Minimum seconds between adaptive concurrency recovery probes",
            ),
            (
                "http-adaptive-recovery-successes-min",
                8,
                1,
                100_000,
                "Minimum successful Ranges before an adaptive concurrency probe",
            ),
            (
                "http-adaptive-recovery-successes-max",
                32,
                1,
                100_000,
                "Maximum successful Ranges before an adaptive concurrency probe",
            ),
            (
                "http-adaptive-start-interval-ms",
                100,
                0,
                60_000,
                "Initial delay in milliseconds between starting HTTP Ranges",
            ),
            (
                "http-adaptive-backoff-base",
                20,
                1,
                3600,
                "Base capacity rejection backoff in seconds",
            ),
            (
                "http-adaptive-backoff-max",
                300,
                1,
                86_400,
                "Maximum capacity rejection backoff in seconds",
            ),
            (
                "http-adaptive-capacity-start-interval-ms",
                1000,
                1,
                60_000,
                "Initial delay in milliseconds between Range starts after capacity rejection",
            ),
            (
                "http-adaptive-capacity-start-interval-max-ms",
                4000,
                1,
                60_000,
                "Maximum delay in milliseconds between Range starts after repeated capacity rejection",
            ),
            (
                "http-adaptive-rate-recovery-successes",
                64,
                1,
                1_000_000,
                "Successful Ranges required before easing request pacing",
            ),
            (
                "http-adaptive-rate-min-interval-ms",
                10,
                0,
                60_000,
                "Minimum delay in milliseconds between Range starts after recovery",
            ),
            (
                "http-adaptive-backoff-reset-after",
                60,
                1,
                86_400,
                "Quiet seconds without capacity rejection before resetting the backoff streak",
            ),
        ] {
            self.register(OptionDef {
                name: name.into(),
                opt_type: OptionType::Integer,
                default_value: OptionValue::Int(default),
                min: Some(min),
                max: Some(max),
                description: description.into(),
                category: OptionCategory::HttpFtp,
                ..Default::default()
            });
        }

        // --- SSL/TLS ---
        self.register(OptionDef {
            name: "check-certificate".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Verify SSL certificate".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ca-certificate".into(),
            opt_type: OptionType::Path,
            description: "CA certificate file".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- File Handling ---
        self.register(OptionDef {
            name: "allow-overwrite".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Allow overwriting existing files".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "auto-file-renaming".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Auto rename conflicting files".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "continue".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('c'),
            default_value: OptionValue::Bool(true),
            description: "Resume partial downloads".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "remote-time".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('R'),
            default_value: OptionValue::Bool(false),
            description: "Use remote file timestamp".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- HTTP Connection Behavior ---
        self.register(OptionDef {
            name: "enable-http-keep-alive".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable HTTP persistent connection (keep-alive)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-http-pipelining".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable HTTP/1.1 pipelining".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-accept-gzip".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Accept gzip-encoded HTTP responses".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-auth-challenge".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Send HTTP authentication header only after challenge".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-no-cache".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Send Cache-Control: no-cache with requests".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "content-disposition-default-utf8".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Treat Content-Disposition filename as UTF-8".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "use-head".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Use HEAD method for file existence checks".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-want-digest-header".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Omit Want-Digest header from HTTP requests".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- HTTP Authentication ---
        self.register(OptionDef {
            name: "http-user".into(),
            opt_type: OptionType::String,
            description: "HTTP authentication username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-passwd".into(),
            opt_type: OptionType::String,
            description: "HTTP authentication password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- SSL/TLS Extended ---
        self.register(OptionDef {
            name: "certificate".into(),
            opt_type: OptionType::Path,
            description: "Client certificate file path (PEM format)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "private-key".into(),
            opt_type: OptionType::Path,
            description: "Client private key file path (PEM format)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "min-tls-version".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("TLSv1.2".into()),
            allowed_values: &["TLSv1.1", "TLSv1.2", "TLSv1.3"],
            description: "Minimum TLS version (TLSv1.1/TLSv1.2/TLSv1.3)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- HTTP Pipelining Extended ---
        self.register(OptionDef {
            name: "max-http-pipelining".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(2),
            min: Some(1),
            max: Some(8),
            description: "Max pipelined HTTP requests per connection".into(),
            category: OptionCategory::HttpFtp,
            supported: false,
            hidden: true,
            ..Default::default()
        });

        // --- FTP Settings ---
        self.register(OptionDef {
            name: "ftp-user".into(),
            opt_type: OptionType::String,
            description: "FTP authentication username".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-passwd".into(),
            opt_type: OptionType::String,
            description: "FTP authentication password".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-pasv".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('p'),
            default_value: OptionValue::Bool(true),
            description: "Use FTP passive mode".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-reuse-connection".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Reuse FTP data connection across downloads".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-type".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("binary".into()),
            allowed_values: &["binary", "ascii"],
            description: "FTP transfer type (binary/ascii)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- SSH / SFTP ---
        self.register(OptionDef {
            name: "ssh-host-key-md".into(),
            opt_type: OptionType::String,
            description:
                "SSH host key fingerprint (hashType=digest format, e.g. sha-1=..., md5=...)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-connect-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::DEFAULT_SFTP_CONNECT_TIMEOUT_SECS as i64,
            ),
            min: Some(1),
            max: Some(600),
            description: "SFTP SSH connection timeout in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-read-timeout".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(aria2_protocol::DEFAULT_SFTP_READ_TIMEOUT_SECS as i64),
            min: Some(1),
            max: Some(600),
            description: "SFTP I/O read timeout in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-compression".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable SSH compression for SFTP connections".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-keepalive-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(
                aria2_protocol::DEFAULT_SFTP_KEEPALIVE_INTERVAL_SECS as i64,
            ),
            min: Some(0),
            max: Some(600),
            description: "SSH keepalive interval in seconds for SFTP; 0 disables keepalives".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-preferred-ciphers".into(),
            opt_type: OptionType::List,
            description: "Comma-separated preferred SSH ciphers for SFTP".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
    }
}
