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
            default_value: OptionValue::Int(16),
            min: Some(1),
            max: Some(16),
            description: "Connections per download".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "min-split-size".into(),
            opt_type: OptionType::Size,
            short_name: Some('k'),
            default_value: OptionValue::Int((20 * 1024 * 1024) as i64),
            min: Some(1024 * 1024),
            max: Some(1024 * 1024 * 1024),
            description: "Min split size".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-connection-per-server".into(),
            opt_type: OptionType::Integer,
            short_name: Some('x'),
            default_value: OptionValue::Int(16),
            min: Some(1),
            max: Some(16),
            description: "HTTP max connections per server (adaptive hard limit)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

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
            default_value: OptionValue::Bool(false),
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
            default_value: OptionValue::Bool(false),
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
            default_value: OptionValue::Bool(false),
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
    }
}
