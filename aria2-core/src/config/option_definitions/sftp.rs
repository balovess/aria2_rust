//! SSH/SFTP connection options.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register SSH/SFTP options using the existing HTTP/FTP CLI category.
    pub(crate) fn register_sftp_options(&mut self) {
        self.register(OptionDef {
            name: "ssh-host-key-md".into(),
            opt_type: OptionType::String,
            description:
                "SSH host key fingerprint (hashType=digest format, e.g. sha-1=..., md5=...)".into(),
            category: OptionCategory::HttpFtp,
            supported: cfg!(feature = "sftp"),
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
            supported: cfg!(feature = "sftp"),
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
            supported: cfg!(feature = "sftp"),
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-compression".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable SSH compression for SFTP connections".into(),
            category: OptionCategory::HttpFtp,
            supported: cfg!(feature = "sftp"),
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
            supported: cfg!(feature = "sftp"),
            ..Default::default()
        });
        self.register(OptionDef {
            name: "sftp-preferred-ciphers".into(),
            opt_type: OptionType::List,
            description: "Comma-separated preferred SSH ciphers for SFTP".into(),
            category: OptionCategory::HttpFtp,
            supported: cfg!(feature = "sftp"),
            ..Default::default()
        });
    }
}
