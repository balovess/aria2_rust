//! Runtime option policy shared by every external adapter.
//!
//! The option names in this module mirror
//! `aria2_original/src/OptionHandlerFactory.cc`. Keeping this metadata in
//! core prevents JSON-RPC, XML-RPC, the C API, and future adapters from
//! drifting into different interpretations of the same wire contract.

/// Options accepted by aria2's `changeGlobalOption` method.
///
/// This is the original aria2 `setChangeGlobalOption(true)` set. Unknown
/// names and known names outside this set must be ignored by RPC adapters.
pub const RUNTIME_GLOBAL_CHANGEABLE_OPTIONS: &[&str] = &[
    // General
    "allow-overwrite",
    "allow-piece-length-change",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
    "check-integrity",
    "conditional-get",
    "continue",
    "dir",
    "download-result",
    "enable-async-dns6",
    "enable-mmap",
    "file-allocation",
    "force-save",
    "save-not-found",
    "hash-check-only",
    "keep-unfinished-download-result",
    "log",
    "log-level",
    "max-concurrent-downloads",
    "max-connection-per-server",
    "max-download-limit",
    "max-download-result",
    "max-mmap-limit",
    "max-overall-download-limit",
    "max-resume-failure-tries",
    "min-split-size",
    "no-file-allocation-limit",
    "optimize-concurrent-downloads",
    "parameterized-uri",
    "pause-metadata",
    "realtime-chunk-checksum",
    "remove-control-file",
    "save-session",
    "rpc-save-upload-metadata",
    // HTTP/FTP
    "connect-timeout",
    "dry-run",
    "lowest-speed-limit",
    "max-file-not-found",
    "max-tries",
    "no-netrc",
    "piece-length",
    "remote-time",
    "retry-wait",
    "reuse-uri",
    "server-stat-of",
    "split",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    "content-disposition-default-utf8",
    "enable-http-keep-alive",
    "enable-http-pipelining",
    "header",
    "http-accept-gzip",
    "http-auth-challenge",
    "http-no-cache",
    "http-passwd",
    "http-user",
    "metalink-location",
    "referer",
    "save-cookies",
    "use-head",
    "no-want-digest-header",
    "user-agent",
    "ftp-passwd",
    "ftp-pasv",
    "ftp-reuse-connection",
    "ftp-type",
    "ftp-user",
    "ssh-host-key-md",
    "http-proxy",
    "http-proxy-passwd",
    "http-proxy-user",
    "https-proxy",
    "https-proxy-passwd",
    "https-proxy-user",
    "ftp-proxy",
    "ftp-proxy-passwd",
    "ftp-proxy-user",
    "all-proxy",
    "all-proxy-passwd",
    "all-proxy-user",
    "no-proxy",
    "proxy-method",
    // BitTorrent
    "bt-enable-hook-after-hash-check",
    "bt-enable-lpd",
    "bt-exclude-tracker",
    "bt-external-ip",
    "bt-force-encryption",
    "bt-hash-check-seed",
    "bt-load-saved-metadata",
    "bt-max-open-files",
    "bt-max-peers",
    "bt-metadata-only",
    "bt-min-crypto-level",
    "bt-prioritize-piece",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "bt-require-crypto",
    "bt-seed-unverified",
    "bt-save-metadata",
    "bt-stop-timeout",
    "bt-tracker",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "enable-peer-exchange",
    "follow-torrent",
    "max-overall-upload-limit",
    "max-upload-limit",
    "seed-time",
    "seed-ratio",
    // Metalink
    "follow-metalink",
    "metalink-base-uri",
    "metalink-enable-unique-protocol",
    "metalink-language",
    "metalink-os",
    "metalink-preferred-protocol",
    "metalink-version",
];

/// Returns whether a name is accepted by `changeGlobalOption`.
pub fn is_global_option_changeable(option_name: &str) -> bool {
    RUNTIME_GLOBAL_CHANGEABLE_OPTIONS.contains(&option_name)
}

/// Options that `aria2.changeOption` applies immediately to active downloads.
///
/// These are the options marked with `setChangeOption(true)` by the original
/// `OptionHandlerFactory`. The reserved-download set below is deliberately a
/// separate policy axis: an option may be accepted for a waiting download but
/// only become effective after an active download is restarted.
pub const RUNTIME_CHANGEABLE_OPTIONS: &[&str] = &[
    "force-save",
    "save-not-found",
    "max-download-limit",
    "bt-max-peers",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "max-upload-limit",
];

/// Options accepted by `aria2.changeOption` for reserved or waiting
/// downloads.
///
/// The list mirrors `setChangeOptionForReserved(true)` in the original
/// implementation. For an active download these options are queued as
/// pending; for a reserved download they take effect immediately.
pub const RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS: &[&str] = &[
    // General
    "allow-overwrite",
    "allow-piece-length-change",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
    "check-integrity",
    "conditional-get",
    "continue",
    "dir",
    "enable-async-dns6",
    "enable-mmap",
    "file-allocation",
    "force-save",
    "save-not-found",
    "hash-check-only",
    "max-connection-per-server",
    "max-download-limit",
    "max-mmap-limit",
    "max-resume-failure-tries",
    "min-split-size",
    "no-file-allocation-limit",
    "pause-metadata",
    "realtime-chunk-checksum",
    "remove-control-file",
    "checksum",
    // HTTP/FTP
    "connect-timeout",
    "lowest-speed-limit",
    "max-file-not-found",
    "max-tries",
    "no-netrc",
    "out",
    "remote-time",
    "retry-wait",
    "reuse-uri",
    "split",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    "content-disposition-default-utf8",
    "enable-http-keep-alive",
    "enable-http-pipelining",
    "header",
    "http-accept-gzip",
    "http-auth-challenge",
    "http-no-cache",
    "http-passwd",
    "http-user",
    "metalink-location",
    "referer",
    "use-head",
    "no-want-digest-header",
    "user-agent",
    "ftp-passwd",
    "ftp-pasv",
    "ftp-reuse-connection",
    "ftp-type",
    "ftp-user",
    "ssh-host-key-md",
    // Proxy
    "http-proxy",
    "http-proxy-passwd",
    "http-proxy-user",
    "https-proxy",
    "https-proxy-passwd",
    "https-proxy-user",
    "ftp-proxy",
    "ftp-proxy-passwd",
    "ftp-proxy-user",
    "all-proxy",
    "all-proxy-passwd",
    "all-proxy-user",
    "no-proxy",
    "proxy-method",
    // Metalink
    "select-file",
    "follow-metalink",
    "metalink-enable-unique-protocol",
    "metalink-language",
    "metalink-os",
    "metalink-preferred-protocol",
    "metalink-version",
    // BitTorrent
    "bt-enable-hook-after-hash-check",
    "bt-enable-lpd",
    "bt-exclude-tracker",
    "bt-external-ip",
    "bt-force-encryption",
    "bt-hash-check-seed",
    "bt-load-saved-metadata",
    "bt-max-peers",
    "bt-metadata-only",
    "bt-min-crypto-level",
    "bt-prioritize-piece",
    "bt-remove-unselected-file",
    "bt-request-peer-speed-limit",
    "bt-require-crypto",
    "bt-seed-unverified",
    "bt-save-metadata",
    "bt-stop-timeout",
    "bt-tracker",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "enable-peer-exchange",
    "follow-torrent",
    "index-out",
    "max-upload-limit",
    "seed-time",
    "seed-ratio",
];

/// Classifies how `aria2.changeOption` applies an option for a download.
pub fn is_option_changeable(option_name: &str, is_active: bool) -> ChangeableKind {
    if RUNTIME_CHANGEABLE_OPTIONS.contains(&option_name) {
        ChangeableKind::Immediate
    } else if RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.contains(&option_name) {
        if is_active {
            ChangeableKind::Pending
        } else {
            ChangeableKind::Immediate
        }
    } else {
        ChangeableKind::NotChangeable
    }
}

/// The application mode for a task-level runtime option update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeableKind {
    /// Option takes effect immediately.
    Immediate,
    /// Option is stored as pending and applied on the next restart.
    Pending,
    /// Option cannot be changed through `aria2.changeOption`.
    NotChangeable,
}

#[cfg(test)]
mod tests {
    use super::{
        ChangeableKind, RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS, RUNTIME_CHANGEABLE_OPTIONS,
        is_global_option_changeable, is_option_changeable,
    };

    #[test]
    fn policy_matches_original_wire_names() {
        assert!(is_global_option_changeable("dir"));
        assert!(is_global_option_changeable("save-session"));
        assert!(is_global_option_changeable("bt-force-encryption"));
        assert!(!is_global_option_changeable("no-conf"));
        assert!(!is_global_option_changeable("show-files"));
    }

    #[test]
    fn task_policy_matches_original_changeability_axes() {
        assert_eq!(RUNTIME_CHANGEABLE_OPTIONS.len(), 7);
        assert_eq!(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.len(), 106);
        assert_eq!(
            is_option_changeable("max-download-limit", true),
            ChangeableKind::Immediate
        );
        assert_eq!(is_option_changeable("dir", true), ChangeableKind::Pending);
        assert_eq!(
            is_option_changeable("dir", false),
            ChangeableKind::Immediate
        );
        assert_eq!(
            is_option_changeable("show-files", false),
            ChangeableKind::NotChangeable
        );
        assert_eq!(
            is_option_changeable("max-retries", false),
            ChangeableKind::NotChangeable
        );
        assert_eq!(
            is_option_changeable("bt-force-encrypt", false),
            ChangeableKind::NotChangeable
        );
        assert_eq!(
            is_option_changeable("bt-detach-seed-only", false),
            ChangeableKind::NotChangeable
        );
    }
}
