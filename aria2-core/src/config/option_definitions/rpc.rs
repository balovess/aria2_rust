//! RPC category options: JSON-RPC/XML-RPC server settings, authentication, CORS, TLS.

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

impl crate::config::OptionRegistry {
    /// Register JSON-RPC/XML-RPC server options: listening, authentication, CORS.
    pub fn register_rpc_options(&mut self) {
        // --- Server Enable / Bind ---
        self.register(OptionDef {
            name: "enable-rpc".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('e'),
            default_value: OptionValue::Bool(false),
            description: "Enable JSON-RPC/XML-RPC server".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-listen-all".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Listen on all network interfaces".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-listen-port".into(),
            opt_type: OptionType::Integer,
            short_name: Some('r'),
            default_value: OptionValue::Int(6800),
            min: Some(1024),
            max: Some(65535),
            description: "RPC server port".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-listen-address".into(),
            opt_type: OptionType::String,
            default_value: OptionValue::Str("127.0.0.1".into()),
            description: "RPC server bind address".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });

        // --- Authentication ---
        self.register(OptionDef {
            name: "rpc-secret".into(),
            opt_type: OptionType::String,
            short_name: Some('I'),
            description: "RPC secret token for authorization".into(),
            category: OptionCategory::Rpc,
            // C++ `GetGlobalOptionRpcMethod` explicitly omits this value.
            expose_in_aria2_rpc: false,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-user".into(),
            opt_type: OptionType::String,
            description: "RPC Basic Auth username".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-passwd".into(),
            opt_type: OptionType::String,
            description: "RPC Basic Auth password".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });

        // --- CORS ---
        self.register(OptionDef {
            name: "rpc-allow-origin".into(),
            opt_type: OptionType::String,
            description: "CORS Allow-Origin value".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-cors-domain".into(),
            opt_type: OptionType::String,
            // aria2_original enables CORS only through an explicit RPC option.
            // Keep this unset so the application does not turn on wildcard
            // CORS merely by loading the built-in defaults.
            default_value: OptionValue::None,
            description: "CORS allowed domains for RPC (comma-separated, * for all)".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });

        // --- HTTPS/TLS ---
        self.register(OptionDef {
            name: "rpc-secure".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable HTTPS for RPC server".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-certificate".into(),
            opt_type: OptionType::Path,
            description: "Path to TLS certificate file (PEM format)".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-private-key".into(),
            opt_type: OptionType::Path,
            description: "Path to TLS private key file (PEM format)".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });

        // --- RPC Additional ---
        self.register(OptionDef {
            name: "rpc-allow-origin-all".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Allow all origins for RPC CORS (Access-Control-Allow-Origin: *)".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-max-request-size".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int((2 * 1024 * 1024) as i64),
            description: "Max RPC request body size".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "rpc-save-upload-metadata".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Save uploaded torrent/metadata files to a directory".into(),
            category: OptionCategory::Rpc,
            ..Default::default()
        });
    }
}
