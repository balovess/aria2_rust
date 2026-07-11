//! Built-in option definitions for aria2-rust.
//!
//! This module contains the registration of all ~77 built-in configuration options,
//! organized by category. Each category has its own registration method on
//! [`OptionRegistry`](super::OptionRegistry) for clear separation of concerns.
//!
//! # Option Priority Categorization (Phase 13 / Wave D — Task D1)
//!
//! Options are classified by how frequently users set them from the CLI:
//!
//! ## P0 — Connection / Timeout / Proxy / Bandwidth (set most often)
//!   General:   dir(d), out(o), input-file(i), quiet(q)
//!   HttpFtp:   timeout(t), connect-timeout(T), max-tries(m), retry-wait(w),
//!             max-connection-per-server(x), min-split-size(k), split(s),
//!             continue(c), all-proxy(p), http-proxy(P), check-certificate(b),
//!             allow-overwrite(O), user-agent(U), referer(R), header(H),
//!             load-cookies(C), no-proxy(N), https-proxy(y)
//!   Advanced:  max-concurrent-downloads(j), max-overall-download-limit(A),
//!             max-download-limit(Q)
//!
//! ## P1 — BT Seeding / RPC / Logging (important but less frequently changed)
//!   General:   log(l), log-level(L), dry-run(n), summary-interval(S)
//!   BitTorrent: seed-ratio(g), seed-time(G), bt-max-peers(B), listen-port(h),
//!             enable-dht(D), follow-torrent(M), bt-force-encryption(X),
//!             bt-save-metadata, enable-peer-exchange, bt-enable-lpd
//!   Rpc:      enable-rpc(e), rpc-listen-port(r), rpc-secret(I), rpc-user,
//!             rpc-passwd
//!   HttpFtp:   ca-certificate(E), save-cookies(V), ftp-proxy(F)
//!   Advanced:  file-allocation(f), max-overall-upload-limit(W),
//!             max-upload-limit(K), disk-cache(Z), piece-length(Y), stop(z)
//!
//! ## P2 — Advanced / Rare (seldom changed from CLI)
//!   General:   conf-path, console-log-level, enable-color, save-session,
//!             save-session-interval, auto-save-interval
//!   HttpFtp:   auto-file-renaming, remote-time
//!   BitTorrent: bt-request-peer-speed-limit, bt-max-open-files,
//!             bt-seed-unverified, bt-min-crypto-level, dht-listen-port,
//!             dht-message-path, on-bt-download-complete, on-bt-download-error
//!   Rpc:      rpc-listen-all, rpc-listen-address, rpc-allow-origin
//!   Advanced:  force-save
//!
//! # Short-Option Mapping (Phase 13 / Wave D — Task D2)
//!
//! | Short | Long Option            | Category | Priority |
//! |-------|------------------------|----------|----------|
//! | d     | dir                    | General  | P0       |
//! | o     | out                    | General  | P0       |
//! | i     | input-file             | General  | P0       |
//! | q     | quiet                  | General  | P0       |
//! | l     | log                    | General  | P1       |
//! | L     | log-level              | General  | P1       |
//! | n     | dry-run                | General  | P2       |
//! | S     | summary-interval       | General  | P2       |
//! | s     | split                  | HttpFtp  | P0       |
//! | c     | continue               | HttpFtp  | P0       |
//! | t     | timeout                | HttpFtp  | P0       |
//! | T     | connect-timeout        | HttpFtp  | P0       |
//! | m     | max-tries              | HttpFtp  | P0       |
//! | w     | retry-wait             | HttpFtp  | P0       |
//! | x     | max-connection-per-server | HttpFtp | P0    |
//! | k     | min-split-size         | HttpFtp  | P0       |
//! | p     | all-proxy              | HttpFtp  | P0       |
//! | P     | http-proxy             | HttpFtp  | P1       |
//! | U     | user-agent             | HttpFtp  | P0       |
//! | R     | referer                | HttpFtp  | P1       |
//! | H     | header                 | HttpFft  | P1       |
//! | b     | check-certificate      | HttpFtp  | P1       |
//! | E     | ca-certificate         | HttpFft  | P2       |
//! | O     | allow-overwrite        | HttpFtp  | P1       |
//! | C     | load-cookies           | HttpFtp  | P1       |
//! | V     | save-cookies           | HttpFft  | P2       |
//! | N     | no-proxy               | HttpFtp  | P1       |
//! | y     | https-proxy            | HttpFft  | P1       |
//! | F     | ftp-proxy              | HttpFft  | P2       |
//! | j     | max-concurrent-downloads | Adv.    | P0       |
//! | f     | file-allocation        | Adv.     | P1       |
//! | z     | stop                   | Adv.     | P2       |
//! | g     | seed-ratio             | BT       | P1       |
//! | G     | seed-time              | BT       | P1       |
//! | B     | bt-max-peers           | BT       | P1       |
//! | h     | listen-port            | BT       | P1       |
//! | D     | enable-dht             | BT       | P1       |
//! | X     | bt-force-encryption    | BT       | P2       |
//! | M     | follow-torrent         | BT       | P1       |
//! | e     | enable-rpc             | RPC      | P1       |
//! | r     | rpc-listen-port        | RPC      | P1       |
//! | I     | rpc-secret             | RPC      | P1       |
//! | A     | max-overall-download-limit | Adv. | P0       |
//! | Q     | max-download-limit     | Adv.     | P0       |
//! | W     | max-overall-upload-limit  | Adv.  | P1       |
//! | K     | max-upload-limit       | Adv.     | P1       |
//! | Z     | disk-cache             | Adv.     | P1       |
//! | Y     | piece-length           | Adv.     | P2       |

use crate::config::{OptionCategory, OptionDef, OptionType, OptionValue};

/// Extension trait that adds categorized registration methods to `OptionRegistry`.
///
/// This trait is implemented for [`super::OptionRegistry`] and provides one method
/// per option category, making it easy to register options in logical groups or
/// to selectively enable/disable categories.
#[allow(dead_code)] // Trait methods are called dynamically via impl blocks
pub(super) trait RegisterOptions {
    /// Register all General category options (directory, logging, UI, session).
    fn register_general_options(&mut self);

    /// Register all HTTP/FTP category options (proxies, headers, timeouts, connections).
    fn register_http_ftp_options(&mut self);

    /// Register all BitTorrent category options (seeding, DHT, PEX, peers).
    fn register_bt_options(&mut self);

    /// Register all RPC category options (JSON-RPC/XML-RPC server settings).
    fn register_rpc_options(&mut self);

    /// Register all Advanced category options (bandwidth limits, disk cache, allocation).
    fn register_advanced_options(&mut self);

    /// Convenience method that registers all categories at once.
    fn register_all_options(&mut self) {
        self.register_general_options();
        self.register_http_ftp_options();
        self.register_bt_options();
        self.register_rpc_options();
        self.register_advanced_options();
    }
}

// Note: The impl block is in option.rs since OptionRegistry is defined there.
// This file only contains the trait definition and is imported by option.rs.

/// ---------------------------------------------------------------------------
/// General Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register general-purpose options: directory, output, logging, UI, session management.
    pub fn register_general_options(&mut self) {
        // --- Directory & Output ---
        self.register(OptionDef {
            name: "dir".into(),
            opt_type: OptionType::Path,
            short_name: Some('d'),
            default_value: OptionValue::Str(".".into()),
            description: "Save directory".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "out".into(),
            opt_type: OptionType::String,
            short_name: Some('o'),
            description: "Output filename".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Logging ---
        self.register(OptionDef {
            name: "log".into(),
            opt_type: OptionType::Path,
            short_name: Some('l'),
            default_value: OptionValue::Str("-".into()),
            description: "Log file path".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "log-level".into(),
            opt_type: OptionType::Enum,
            short_name: Some('L'),
            default_value: OptionValue::Str("info".into()),
            description: "Log level (debug/info/notice/warn/error)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "console-log-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("notice".into()),
            description: "Console log level".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Progress & Intervals ---
        self.register(OptionDef {
            name: "summary-interval".into(),
            opt_type: OptionType::Integer,
            short_name: Some('S'),
            default_value: OptionValue::Int(60),
            min: Some(0),
            max: Some(3600),
            description: "Progress summary interval in seconds".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Configuration Files ---
        self.register(OptionDef {
            name: "conf-path".into(),
            opt_type: OptionType::Path,
            description: "Configuration file path".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "input-file".into(),
            opt_type: OptionType::Path,
            short_name: Some('i'),
            description: "URI input file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Session Management ---
        self.register(OptionDef {
            name: "save-session".into(),
            opt_type: OptionType::Path,
            description: "Session save file".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-session-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            description: "Auto-save session interval (0=disabled)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "auto-save-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(60),
            min: Some(0),
            max: Some(600),
            description: "Auto-save interval".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- UI Behavior ---
        self.register(OptionDef {
            name: "enable-color".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable colored output".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "quiet".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('q'),
            default_value: OptionValue::Bool(false),
            description: "Quiet mode".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dry-run".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('n'),
            default_value: OptionValue::Bool(false),
            description: "Dry run (check only, no download)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });

        // --- Daemon Mode ---
        self.register(OptionDef {
            name: "daemon".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('D'),
            default_value: OptionValue::Bool(false),
            description: "Run as a background daemon (detached process)".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "pid-file".into(),
            opt_type: OptionType::Path,
            description: "Path to PID file for daemon process management".into(),
            category: OptionCategory::General,
            ..Default::default()
        });
    }
}

/// ---------------------------------------------------------------------------
/// HTTP/FTP Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register HTTP/FTP download options: proxies, headers, timeouts, connection management.
    pub fn register_http_ftp_options(&mut self) {
        // --- Proxy Settings ---
        self.register(OptionDef {
            name: "all-proxy".into(),
            opt_type: OptionType::String,
            short_name: Some('p'),
            description: "Global proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "http-proxy".into(),
            opt_type: OptionType::String,
            short_name: Some('P'),
            description: "HTTP proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "https-proxy".into(),
            opt_type: OptionType::String,
            short_name: Some('y'),
            description: "HTTPS proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ftp-proxy".into(),
            opt_type: OptionType::String,
            short_name: Some('F'),
            description: "FTP proxy URL".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "no-proxy".into(),
            opt_type: OptionType::List,
            short_name: Some('N'),
            description: "Proxy exclusion list (comma-separated domains)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- HTTP Headers & Identity ---
        self.register(OptionDef {
            name: "user-agent".into(),
            opt_type: OptionType::String,
            short_name: Some('U'),
            default_value: OptionValue::Str("aria2/1.37.0-Rust".into()),
            description: "User-Agent header".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "referer".into(),
            opt_type: OptionType::String,
            short_name: Some('R'),
            description: "Referer header".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "header".into(),
            opt_type: OptionType::List,
            short_name: Some('H'),
            description: "Custom headers (Header:Value pairs)".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Cookies ---
        self.register(OptionDef {
            name: "load-cookies".into(),
            opt_type: OptionType::Path,
            short_name: Some('C'),
            description: "Cookie file to load".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-cookies".into(),
            opt_type: OptionType::Path,
            short_name: Some('V'),
            description: "Cookie file to save".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Timeouts & Retries ---
        self.register(OptionDef {
            name: "connect-timeout".into(),
            opt_type: OptionType::Integer,
            short_name: Some('T'),
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
            max: Some(100),
            description: "Max retry attempts".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "retry-wait".into(),
            opt_type: OptionType::Integer,
            short_name: Some('w'),
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(3600),
            description: "Retry wait time in seconds".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- Connection Management ---
        self.register(OptionDef {
            name: "split".into(),
            opt_type: OptionType::Integer,
            short_name: Some('s'),
            default_value: OptionValue::Int(5),
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
            description: "Min split size".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-connection-per-server".into(),
            opt_type: OptionType::Integer,
            short_name: Some('x'),
            default_value: OptionValue::Int(1),
            min: Some(1),
            max: Some(16),
            description: "Max connections per server".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- SSL/TLS ---
        self.register(OptionDef {
            name: "check-certificate".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('b'),
            default_value: OptionValue::Bool(true),
            description: "Verify SSL certificate".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "ca-certificate".into(),
            opt_type: OptionType::Path,
            short_name: Some('E'),
            description: "CA certificate file".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });

        // --- File Handling ---
        self.register(OptionDef {
            name: "allow-overwrite".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('O'),
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
            default_value: OptionValue::Bool(true),
            description: "Use remote file timestamp".into(),
            category: OptionCategory::HttpFtp,
            ..Default::default()
        });
    }
}

/// ---------------------------------------------------------------------------
/// BitTorrent Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register BitTorrent-specific options: seeding, DHT, PEX, peer management.
    pub fn register_bt_options(&mut self) {
        // --- Seeding Settings ---
        self.register(OptionDef {
            name: "seed-time".into(),
            opt_type: OptionType::Float,
            short_name: Some('G'),
            default_value: OptionValue::Float(0.0),
            description: "Seeding time in minutes (0=infinite)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "seed-ratio".into(),
            opt_type: OptionType::Float,
            short_name: Some('g'),
            default_value: OptionValue::Float(1.0),
            description: "Share ratio threshold".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Peer Management ---
        self.register(OptionDef {
            name: "bt-max-peers".into(),
            opt_type: OptionType::Integer,
            short_name: Some('B'),
            default_value: OptionValue::Int(55),
            min: Some(0),
            max: Some(512),
            description: "Max peers per torrent".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-request-peer-speed-limit".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int((50 * 1024) as i64),
            description: "Min peer speed to stay connected".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-max-open-files".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(100),
            min: Some(10),
            max: Some(4096),
            description: "Max open files for BT".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Torrent Behavior ---
        self.register(OptionDef {
            name: "bt-seed-unverified".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Seed without verifying hash".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-save-metadata".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('M'),
            default_value: OptionValue::Bool(false),
            description: "Save metadata as .torrent file".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Encryption ---
        self.register(OptionDef {
            name: "bt-force-encryption".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('X'),
            default_value: OptionValue::Bool(false),
            description: "Force BT encryption".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-min-crypto-level".into(),
            opt_type: OptionType::Enum,
            default_value: OptionValue::Str("plain".into()),
            description: "Min crypto level (plain/arc4)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- DHT / LPD / PEX ---
        self.register(OptionDef {
            name: "bt-enable-lpd".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable Local Peer Discovery".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-lpd".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable Local Peer Discovery (alias for bt-enable-lpd)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "lpd-listen-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(6771),
            min: Some(1024),
            max: Some(65535),
            description: "UDP port for Local Peer Discovery".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "bt-enable-web-seed".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable web seed (HTTP/FTP seeding)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-dht".into(),
            opt_type: OptionType::Boolean,
            short_name: Some('D'),
            default_value: OptionValue::Bool(true),
            description: "Enable DHT".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-listen-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(6881),
            min: Some(1024),
            max: Some(65535),
            description: "DHT listen port".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-entry-point".into(),
            opt_type: OptionType::List,
            description: "DHT bootstrap nodes (host:port format, comma-separated)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-file-path".into(),
            opt_type: OptionType::Path,
            description: "Path to DHT routing table file for persistence".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "dht-message-path".into(),
            opt_type: OptionType::Path,
            description: "DHT message cache path (deprecated, use dht-file-path instead)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "enable-peer-exchange".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(true),
            description: "Enable PEX".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Torrent Handling ---
        self.register(OptionDef {
            name: "follow-torrent".into(),
            opt_type: OptionType::Enum,
            short_name: Some('M'),
            default_value: OptionValue::Str("true".into()),
            description: "Auto-handle .torrent (true/false/mem)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Event Hooks ---
        self.register(OptionDef {
            name: "on-bt-download-complete".into(),
            opt_type: OptionType::String,
            description: "Command on BT download complete".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "on-bt-download-error".into(),
            opt_type: OptionType::String,
            description: "Command on BT download error".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Listening Port ---
        self.register(OptionDef {
            name: "listen-port".into(),
            opt_type: OptionType::String,
            short_name: Some('h'),
            default_value: OptionValue::Str("6881-6999".into()),
            description: "Listening port range".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- Piece Selection Priority (G2) ---
        self.register(OptionDef {
            name: "bt-prioritize-piece".into(),
            opt_type: OptionType::String,
            default_value: OptionValue::Str("rarest".into()),
            description: "Piece selection priority mode: 'rarest' (default), 'head' (sequential from start), 'tail' (sequential from end)".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });

        // --- uTP (UDP Transport Protocol - BEP 29) ---
        // Note: uTP is not implemented in the original C++ aria2. This is an experimental
        // feature in aria2-rust that implements BEP 29 (http://www.bittorrent.org/beps/bep_0029.html).
        // uTP provides congestion control over UDP, making BitTorrent friendlier to network traffic.
        self.register(OptionDef {
            name: "enable-utp".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Enable uTP (UDP Transport Protocol, BEP 29). Experimental feature not in original aria2. Default: false".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "utp-listen-port".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(65535),
            description: "UDP port for uTP connections. 0 = auto-assign. Experimental feature not in original aria2".into(),
            category: OptionCategory::BitTorrent,
            ..Default::default()
        });
    }
}

/// ---------------------------------------------------------------------------
/// RPC Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
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
            default_value: OptionValue::Str("*".into()),
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
    }
}

/// ---------------------------------------------------------------------------
/// Advanced Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register advanced/performance options: bandwidth limits, disk cache, file allocation.
    pub fn register_advanced_options(&mut self) {
        // --- File Allocation ---
        self.register(OptionDef {
            name: "file-allocation".into(),
            opt_type: OptionType::Enum,
            short_name: Some('f'),
            default_value: OptionValue::Str("falloc".into()),
            description: "File allocation method (none/prealloc/falloc/trunc/mmap)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Secure Falloc ---
        // Zero-fill allocated space after fallocate on platforms that don't
        // zero-fill (macOS F_PREALLOCATE, Windows SetFileValidData). Prevents
        // exposure of residual disk data at a performance cost. Has no effect
        // on Linux where fallocate(2) always returns zeroed blocks.
        self.register(OptionDef {
            name: "secure-falloc".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Zero-fill allocated space after fallocate on platforms that don't zero-fill (macOS, Windows). Prevents exposure of residual disk data at a performance cost.".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Mmap Threshold ---
        // Files larger than this threshold use MmapDiskWriter when
        // --file-allocation=mmap is set. Below the threshold, positioned I/O
        // is used (avoids address space waste for small files).
        self.register(OptionDef {
            name: "mmap-threshold".into(),
            opt_type: OptionType::Size,
            default_value: OptionValue::Int(256 * 1024 * 1024), // 256 MiB
            description: "File size threshold for mmap writes when file-allocation=mmap (default 256 MiB)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Concurrency ---
        self.register(OptionDef {
            name: "max-concurrent-downloads".into(),
            opt_type: OptionType::Integer,
            short_name: Some('j'),
            default_value: OptionValue::Int(5),
            min: Some(1),
            max: Some(256),
            description: "Max concurrent downloads".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Bandwidth Limits ---
        self.register(OptionDef {
            name: "max-overall-download-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('A'),
            default_value: OptionValue::Int(0),
            description: "Overall download speed limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-download-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('Q'),
            default_value: OptionValue::Int(0),
            description: "Per-task download limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-overall-upload-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('W'),
            default_value: OptionValue::Int(0),
            description: "Overall upload speed limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "max-upload-limit".into(),
            opt_type: OptionType::Size,
            short_name: Some('K'),
            default_value: OptionValue::Int(0),
            description: "Per-task upload limit (0=unlimited)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- BT Piece & Disk ---
        self.register(OptionDef {
            name: "piece-length".into(),
            opt_type: OptionType::Size,
            short_name: Some('Y'),
            default_value: OptionValue::Int((1024 * 1024) as i64),
            description: "BT piece length".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "disk-cache".into(),
            opt_type: OptionType::Size,
            short_name: Some('Z'),
            default_value: OptionValue::Int(0),
            description: "Disk cache size (0=disabled)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Auto-stop & Save ---
        self.register(OptionDef {
            name: "stop".into(),
            opt_type: OptionType::Integer,
            short_name: Some('z'),
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(86400),
            description: "Stop after N seconds of completion (0=never)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "force-save".into(),
            opt_type: OptionType::Boolean,
            default_value: OptionValue::Bool(false),
            description: "Force save state on every change".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });

        // --- Server Statistics Persistence ---
        self.register(OptionDef {
            name: "server-stat-file".into(),
            opt_type: OptionType::Path,
            description: "Path to save/load server performance statistics".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
        self.register(OptionDef {
            name: "save-server-stat-interval".into(),
            opt_type: OptionType::Integer,
            default_value: OptionValue::Int(0),
            min: Some(0),
            max: Some(86400),
            description: "Auto-save interval for server stats in seconds (0 = disabled)".into(),
            category: OptionCategory::Advanced,
            ..Default::default()
        });
    }
}
