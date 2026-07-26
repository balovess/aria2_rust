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

mod advanced;
#[cfg(feature = "bittorrent")]
mod bittorrent;
mod general;
mod http_ftp;
mod rpc;

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
    #[cfg(feature = "bittorrent")]
    fn register_bt_options(&mut self);

    /// Register all RPC category options (JSON-RPC/XML-RPC server settings).
    fn register_rpc_options(&mut self);

    /// Register all Advanced category options (bandwidth limits, disk cache, allocation).
    fn register_advanced_options(&mut self);

    /// Convenience method that registers all categories at once.
    fn register_all_options(&mut self) {
        self.register_general_options();
        self.register_http_ftp_options();
        #[cfg(feature = "bittorrent")]
        self.register_bt_options();
        self.register_rpc_options();
        self.register_advanced_options();
    }
}

// Note: The impl RegisterOptions for OptionRegistry block is in option.rs
// since OptionRegistry is defined there. The individual register_*_options
// methods are defined in the category sub-modules (general.rs, http_ftp.rs, etc.)
// as separate `impl Option
