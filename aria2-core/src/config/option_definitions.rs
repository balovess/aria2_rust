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
        self.register(
            OptionDef::new("dir", OptionType::Path)
                .short('d')
                .default(OptionValue::Str(".".into()))
                .desc("Save directory")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("out", OptionType::String)
                .short('o')
                .desc("Output filename")
                .category(OptionCategory::General),
        );

        // --- Logging ---
        self.register(
            OptionDef::new("log", OptionType::Path)
                .short('l')
                .default(OptionValue::Str("-".into()))
                .desc("Log file path")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("log-level", OptionType::Enum)
                .short('L')
                .default(OptionValue::Str("info".into()))
                .desc("Log level (debug/info/notice/warn/error)")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("console-log-level", OptionType::Enum)
                .default(OptionValue::Str("notice".into()))
                .desc("Console log level")
                .category(OptionCategory::General),
        );

        // --- Progress & Intervals ---
        self.register(
            OptionDef::new("summary-interval", OptionType::Integer)
                .short('S')
                .default(OptionValue::Int(60))
                .range(0, 3600)
                .desc("Progress summary interval in seconds")
                .category(OptionCategory::General),
        );

        // --- Configuration Files ---
        self.register(
            OptionDef::new("conf-path", OptionType::Path)
                .desc("Configuration file path")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("input-file", OptionType::Path)
                .short('i')
                .desc("URI input file")
                .category(OptionCategory::General),
        );

        // --- Session Management ---
        self.register(
            OptionDef::new("save-session", OptionType::Path)
                .desc("Session save file")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("save-session-interval", OptionType::Integer)
                .default(OptionValue::Int(0))
                .desc("Auto-save session interval (0=disabled)")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("auto-save-interval", OptionType::Integer)
                .default(OptionValue::Int(60))
                .range(0, 600)
                .desc("Auto-save interval")
                .category(OptionCategory::General),
        );

        // --- UI Behavior ---
        self.register(
            OptionDef::new("enable-color", OptionType::Boolean)
                .default(OptionValue::Bool(true))
                .desc("Enable colored output")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("quiet", OptionType::Boolean)
                .short('q')
                .default(OptionValue::Bool(false))
                .desc("Quiet mode")
                .category(OptionCategory::General),
        );
        self.register(
            OptionDef::new("dry-run", OptionType::Boolean)
                .short('n')
                .default(OptionValue::Bool(false))
                .desc("Dry run (check only, no download)")
                .category(OptionCategory::General),
        );
    }
}

/// ---------------------------------------------------------------------------
/// HTTP/FTP Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register HTTP/FTP download options: proxies, headers, timeouts, connection management.
    pub fn register_http_ftp_options(&mut self) {
        // --- Proxy Settings ---
        self.register(
            OptionDef::new("all-proxy", OptionType::String)
                .short('p')
                .desc("Global proxy URL")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("http-proxy", OptionType::String)
                .short('P')
                .desc("HTTP proxy URL")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("https-proxy", OptionType::String)
                .short('y')
                .desc("HTTPS proxy URL")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("ftp-proxy", OptionType::String)
                .short('F')
                .desc("FTP proxy URL")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("no-proxy", OptionType::List)
                .short('N')
                .desc("Proxy exclusion list (comma-separated domains)")
                .category(OptionCategory::HttpFtp),
        );

        // --- HTTP Headers & Identity ---
        self.register(
            OptionDef::new("user-agent", OptionType::String)
                .short('U')
                .default(OptionValue::Str("aria2/1.37.0-Rust".into()))
                .desc("User-Agent header")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("referer", OptionType::String)
                .short('R')
                .desc("Referer header")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("header", OptionType::List)
                .short('H')
                .desc("Custom headers (Header:Value pairs)")
                .category(OptionCategory::HttpFtp),
        );

        // --- Cookies ---
        self.register(
            OptionDef::new("load-cookies", OptionType::Path)
                .short('C')
                .desc("Cookie file to load")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("save-cookies", OptionType::Path)
                .short('V')
                .desc("Cookie file to save")
                .category(OptionCategory::HttpFtp),
        );

        // --- Timeouts & Retries ---
        self.register(
            OptionDef::new("connect-timeout", OptionType::Integer)
                .short('T')
                .default(OptionValue::Int(60))
                .range(1, 600)
                .desc("Connect timeout in seconds")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("timeout", OptionType::Integer)
                .short('t')
                .default(OptionValue::Int(60))
                .range(1, 600)
                .desc("I/O timeout in seconds")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("max-tries", OptionType::Integer)
                .short('m')
                .default(OptionValue::Int(5))
                .range(0, 100)
                .desc("Max retry attempts")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("retry-wait", OptionType::Integer)
                .short('w')
                .default(OptionValue::Int(0))
                .range(0, 3600)
                .desc("Retry wait time in seconds")
                .category(OptionCategory::HttpFtp),
        );

        // --- Connection Management ---
        self.register(
            OptionDef::new("split", OptionType::Integer)
                .short('s')
                .default(OptionValue::Int(5))
                .range(1, 16)
                .desc("Connections per download")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("min-split-size", OptionType::Size)
                .short('k')
                .default(OptionValue::Int((20 * 1024 * 1024) as i64))
                .desc("Min split size")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("max-connection-per-server", OptionType::Integer)
                .short('x')
                .default(OptionValue::Int(1))
                .range(1, 16)
                .desc("Max connections per server")
                .category(OptionCategory::HttpFtp),
        );

        // --- SSL/TLS ---
        self.register(
            OptionDef::new("check-certificate", OptionType::Boolean)
                .short('b')
                .default(OptionValue::Bool(true))
                .desc("Verify SSL certificate")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("ca-certificate", OptionType::Path)
                .short('E')
                .desc("CA certificate file")
                .category(OptionCategory::HttpFtp),
        );

        // --- File Handling ---
        self.register(
            OptionDef::new("allow-overwrite", OptionType::Boolean)
                .short('O')
                .default(OptionValue::Bool(false))
                .desc("Allow overwriting existing files")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("auto-file-renaming", OptionType::Boolean)
                .default(OptionValue::Bool(true))
                .desc("Auto rename conflicting files")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("continue", OptionType::Boolean)
                .short('c')
                .default(OptionValue::Bool(true))
                .desc("Resume partial downloads")
                .category(OptionCategory::HttpFtp),
        );
        self.register(
            OptionDef::new("remote-time", OptionType::Boolean)
                .default(OptionValue::Bool(true))
                .desc("Use remote file timestamp")
                .category(OptionCategory::HttpFtp),
        );
    }
}

/// ---------------------------------------------------------------------------
/// BitTorrent Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register BitTorrent-specific options: seeding, DHT, PEX, peer management.
    pub fn register_bt_options(&mut self) {
        // --- Seeding Settings ---
        self.register(
            OptionDef::new("seed-time", OptionType::Float)
                .short('G')
                .default(OptionValue::Float(0.0))
                .desc("Seeding time in minutes (0=infinite)")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("seed-ratio", OptionType::Float)
                .short('g')
                .default(OptionValue::Float(1.0))
                .desc("Share ratio threshold")
                .category(OptionCategory::BitTorrent),
        );

        // --- Peer Management ---
        self.register(
            OptionDef::new("bt-max-peers", OptionType::Integer)
                .short('B')
                .default(OptionValue::Int(55))
                .range(0, 512)
                .desc("Max peers per torrent")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("bt-request-peer-speed-limit", OptionType::Size)
                .default(OptionValue::Int((50 * 1024) as i64))
                .desc("Min peer speed to stay connected")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("bt-max-open-files", OptionType::Integer)
                .default(OptionValue::Int(100))
                .range(10, 4096)
                .desc("Max open files for BT")
                .category(OptionCategory::BitTorrent),
        );

        // --- Torrent Behavior ---
        self.register(
            OptionDef::new("bt-seed-unverified", OptionType::Boolean)
                .default(OptionValue::Bool(false))
                .desc("Seed without verifying hash")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("bt-save-metadata", OptionType::Boolean)
                .short('M')
                .default(OptionValue::Bool(false))
                .desc("Save metadata as .torrent file")
                .category(OptionCategory::BitTorrent),
        );

        // --- Encryption ---
        self.register(
            OptionDef::new("bt-force-encryption", OptionType::Boolean)
                .short('X')
                .default(OptionValue::Bool(false))
                .desc("Force BT encryption")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("bt-min-crypto-level", OptionType::Enum)
                .default(OptionValue::Str("plain".into()))
                .desc("Min crypto level (plain/arc4)")
                .category(OptionCategory::BitTorrent),
        );

        // --- DHT / LPD / PEX ---
        self.register(
            OptionDef::new("bt-enable-lpd", OptionType::Boolean)
                .default(OptionValue::Bool(false))
                .desc("Enable Local Peer Discovery")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("enable-dht", OptionType::Boolean)
                .short('D')
                .default(OptionValue::Bool(true))
                .desc("Enable DHT")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("dht-listen-port", OptionType::Integer)
                .default(OptionValue::Int(6881))
                .range(1024, 65535)
                .desc("DHT listen port")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("dht-message-path", OptionType::Path)
                .desc("DHT message cache path")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("enable-peer-exchange", OptionType::Boolean)
                .default(OptionValue::Bool(true))
                .desc("Enable PEX")
                .category(OptionCategory::BitTorrent),
        );

        // --- Torrent Handling ---
        self.register(
            OptionDef::new("follow-torrent", OptionType::Enum)
                .short('M')
                .default(OptionValue::Str("true".into()))
                .desc("Auto-handle .torrent (true/false/mem)")
                .category(OptionCategory::BitTorrent),
        );

        // --- Event Hooks ---
        self.register(
            OptionDef::new("on-bt-download-complete", OptionType::String)
                .desc("Command on BT download complete")
                .category(OptionCategory::BitTorrent),
        );
        self.register(
            OptionDef::new("on-bt-download-error", OptionType::String)
                .desc("Command on BT download error")
                .category(OptionCategory::BitTorrent),
        );

        // --- Listening Port ---
        self.register(
            OptionDef::new("listen-port", OptionType::String)
                .short('h')
                .default(OptionValue::Str("6881-6999".into()))
                .desc("Listening port range")
                .category(OptionCategory::BitTorrent),
        );

        // --- Piece Selection Priority (G2) ---
        self.register(
            OptionDef::new("bt-prioritize-piece", OptionType::String)
                .default(OptionValue::Str("rarest".into()))
                .desc("Piece selection priority mode: 'rarest' (default), 'head' (sequential from start), 'tail' (sequential from end)")
                .category(OptionCategory::BitTorrent),
        );
    }
}

/// ---------------------------------------------------------------------------
/// RPC Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register JSON-RPC/XML-RPC server options: listening, authentication, CORS.
    pub fn register_rpc_options(&mut self) {
        // --- Server Enable / Bind ---
        self.register(
            OptionDef::new("enable-rpc", OptionType::Boolean)
                .short('e')
                .default(OptionValue::Bool(false))
                .desc("Enable JSON-RPC/XML-RPC server")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-listen-all", OptionType::Boolean)
                .default(OptionValue::Bool(false))
                .desc("Listen on all network interfaces")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-listen-port", OptionType::Integer)
                .short('r')
                .default(OptionValue::Int(6800))
                .range(1024, 65535)
                .desc("RPC server port")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-listen-address", OptionType::String)
                .default(OptionValue::Str("127.0.0.1".into()))
                .desc("RPC server bind address")
                .category(OptionCategory::Rpc),
        );

        // --- Authentication ---
        self.register(
            OptionDef::new("rpc-secret", OptionType::String)
                .short('I')
                .desc("RPC secret token for authorization")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-user", OptionType::String)
                .desc("RPC Basic Auth username")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-passwd", OptionType::String)
                .desc("RPC Basic Auth password")
                .category(OptionCategory::Rpc),
        );

        // --- CORS ---
        self.register(
            OptionDef::new("rpc-allow-origin", OptionType::String)
                .desc("CORS Allow-Origin value")
                .category(OptionCategory::Rpc),
        );
        self.register(
            OptionDef::new("rpc-cors-domain", OptionType::String)
                .default(OptionValue::Str("*".into()))
                .desc("CORS allowed domains for RPC (comma-separated, * for all)")
                .category(OptionCategory::Rpc),
        );
    }
}

/// ---------------------------------------------------------------------------
/// Advanced Options
/// ---------------------------------------------------------------------------
impl super::OptionRegistry {
    /// Register advanced/performance options: bandwidth limits, disk cache, file allocation.
    pub fn register_advanced_options(&mut self) {
        // --- File Allocation ---
        self.register(
            OptionDef::new("file-allocation", OptionType::Enum)
                .short('f')
                .default(OptionValue::Str("prealloc".into()))
                .desc("File allocation method (none/prealloc/falloc/trunc)")
                .category(OptionCategory::Advanced),
        );

        // --- Concurrency ---
        self.register(
            OptionDef::new("max-concurrent-downloads", OptionType::Integer)
                .short('j')
                .default(OptionValue::Int(5))
                .range(1, 256)
                .desc("Max concurrent downloads")
                .category(OptionCategory::Advanced),
        );

        // --- Bandwidth Limits ---
        self.register(
            OptionDef::new("max-overall-download-limit", OptionType::Size)
                .short('A')
                .default(OptionValue::Int(0))
                .desc("Overall download speed limit (0=unlimited)")
                .category(OptionCategory::Advanced),
        );
        self.register(
            OptionDef::new("max-download-limit", OptionType::Size)
                .short('Q')
                .default(OptionValue::Int(0))
                .desc("Per-task download limit (0=unlimited)")
                .category(OptionCategory::Advanced),
        );
        self.register(
            OptionDef::new("max-overall-upload-limit", OptionType::Size)
                .short('W')
                .default(OptionValue::Int(0))
                .desc("Overall upload speed limit (0=unlimited)")
                .category(OptionCategory::Advanced),
        );
        self.register(
            OptionDef::new("max-upload-limit", OptionType::Size)
                .short('K')
                .default(OptionValue::Int(0))
                .desc("Per-task upload limit (0=unlimited)")
                .category(OptionCategory::Advanced),
        );

        // --- BT Piece & Disk ---
        self.register(
            OptionDef::new("piece-length", OptionType::Size)
                .short('Y')
                .default(OptionValue::Int((1024 * 1024) as i64))
                .desc("BT piece length")
                .category(OptionCategory::Advanced),
        );
        self.register(
            OptionDef::new("disk-cache", OptionType::Size)
                .short('Z')
                .default(OptionValue::Int(0))
                .desc("Disk cache size (0=disabled)")
                .category(OptionCategory::Advanced),
        );

        // --- Auto-stop & Save ---
        self.register(
            OptionDef::new("stop", OptionType::Integer)
                .short('z')
                .default(OptionValue::Int(0))
                .range(0, 86400)
                .desc("Stop after N seconds of completion (0=never)")
                .category(OptionCategory::Advanced),
        );
        self.register(
            OptionDef::new("force-save", OptionType::Boolean)
                .default(OptionValue::Bool(false))
                .desc("Force save state on every change")
                .category(OptionCategory::Advanced),
        );
    }
}
