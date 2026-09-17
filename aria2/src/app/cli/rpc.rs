use std::path::PathBuf;

use clap::Args;

// =========================================================================
// RPC Options
// =========================================================================

/// JSON-RPC/XML-RPC server options.
#[derive(Args, Debug)]
#[command(next_help_heading = "RPC options")]
pub struct RpcArgs {
    /// Enable JSON-RPC/XML-RPC server
    #[arg(
        short = 'e',
        long = "enable-rpc",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub enable_rpc: Option<bool>,

    /// Listen on all network interfaces
    #[arg(
        long = "rpc-listen-all",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_listen_all: Option<bool>,

    /// RPC server port
    #[arg(short = 'r', long = "rpc-listen-port")]
    pub rpc_listen_port: Option<u16>,

    /// RPC server bind address
    #[arg(long = "rpc-listen-address")]
    pub rpc_listen_address: Option<String>,

    /// RPC secret token for authorization
    #[arg(short = 'I', long = "rpc-secret")]
    pub rpc_secret: Option<String>,

    /// RPC Basic Auth username
    #[arg(long = "rpc-user")]
    pub rpc_user: Option<String>,

    /// RPC Basic Auth password
    #[arg(long = "rpc-passwd")]
    pub rpc_passwd: Option<String>,

    /// CORS Allow-Origin value
    #[arg(long = "rpc-allow-origin")]
    pub rpc_allow_origin: Option<String>,

    /// CORS allowed domains for RPC (comma-separated)
    #[arg(long = "rpc-cors-domain")]
    pub rpc_cors_domain: Option<String>,

    /// Enable HTTPS for RPC server
    #[arg(
        long = "rpc-secure",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_secure: Option<bool>,

    /// Path to TLS certificate file (PEM format)
    #[arg(long = "rpc-certificate")]
    pub rpc_certificate: Option<PathBuf>,

    /// Path to TLS private key file (PEM format)
    #[arg(long = "rpc-private-key")]
    pub rpc_private_key: Option<PathBuf>,

    /// Allow all origins for RPC CORS (Access-Control-Allow-Origin: *)
    #[arg(
        long = "rpc-allow-origin-all",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_allow_origin_all: Option<bool>,

    /// Max RPC request body size
    #[arg(long = "rpc-max-request-size")]
    pub rpc_max_request_size: Option<String>,

    /// Save uploaded torrent/metadata files to a directory
    #[arg(
        long = "rpc-save-upload-metadata",
        num_args(0..=1),
        require_equals = true,
        default_missing_value = "true",
        value_name = "true|false"
    )]
    pub rpc_save_upload_metadata: Option<bool>,
}
