//! Runtime option policy shared by every external adapter.
//!
//! The option names in this module define one compatibility policy shared by
//! every external adapter. Keeping this metadata in core prevents JSON-RPC,
//! XML-RPC, the C API, and future adapters from drifting into different
//! interpretations of the same wire contract.

use std::collections::HashMap;

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
    "disk-cache",
    "download-result",
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
    #[cfg(feature = "bittorrent")]
    "enable-public-trackers",
    #[cfg(feature = "bittorrent")]
    "bt-tracker-source",
    #[cfg(feature = "bittorrent")]
    "bt-tracker-update-interval",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "bt-tracker-stopped-timeout",
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

/// Options copied into a request group when aria2 creates a download.
///
/// This is the original aria2 `setInitialOption(true)` set from
/// `OptionHandlerFactory.cc`. `aria2.getOption` serializes only these values
/// from a group's option state; RPC listener, authentication, logging, and
/// other process-wide settings must never become task options.
pub const INITIAL_REQUEST_OPTIONS: &[&str] = &[
    "allow-overwrite",
    "allow-piece-length-change",
    "all-proxy",
    "all-proxy-passwd",
    "all-proxy-user",
    "always-resume",
    "async-dns",
    "auto-file-renaming",
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
    "bt-save-metadata",
    "bt-seed-unverified",
    "bt-stop-timeout",
    "bt-tracker",
    "bt-tracker-connect-timeout",
    "bt-tracker-interval",
    "bt-tracker-timeout",
    "bt-tracker-stopped-timeout",
    "check-integrity",
    "checksum",
    "conditional-get",
    "connect-timeout",
    "content-disposition-default-utf8",
    "continue",
    "dir",
    "disk-cache",
    "dry-run",
    "enable-http-keep-alive",
    "enable-http-pipelining",
    "enable-mmap",
    "enable-peer-exchange",
    "file-allocation",
    "follow-metalink",
    "follow-torrent",
    "force-save",
    "ftp-passwd",
    "ftp-pasv",
    "ftp-proxy",
    "ftp-proxy-passwd",
    "ftp-proxy-user",
    "ftp-reuse-connection",
    "ftp-type",
    "ftp-user",
    "gid",
    "hash-check-only",
    "header",
    "http-accept-gzip",
    "http-auth-challenge",
    "http-no-cache",
    "http-passwd",
    "http-proxy",
    "http-proxy-passwd",
    "http-proxy-user",
    "http-user",
    "https-proxy",
    "https-proxy-passwd",
    "https-proxy-user",
    "index-out",
    "lowest-speed-limit",
    "max-connection-per-server",
    "max-download-limit",
    "max-file-not-found",
    "max-mmap-limit",
    "max-resume-failure-tries",
    "max-tries",
    "max-upload-limit",
    "metalink-base-uri",
    "metalink-enable-unique-protocol",
    "metalink-language",
    "metalink-location",
    "metalink-os",
    "metalink-preferred-protocol",
    "metalink-version",
    "min-split-size",
    "no-file-allocation-limit",
    "no-netrc",
    "no-proxy",
    "no-want-digest-header",
    "out",
    "parameterized-uri",
    "pause",
    "pause-metadata",
    "piece-length",
    "proxy-method",
    "realtime-chunk-checksum",
    "referer",
    "remote-time",
    "remove-control-file",
    "retry-wait",
    "reuse-uri",
    "rpc-save-upload-metadata",
    "save-not-found",
    "seed-ratio",
    "seed-time",
    "select-file",
    "split",
    "ssh-host-key-md",
    "stream-piece-selector",
    "timeout",
    "uri-selector",
    "use-head",
    "user-agent",
];

/// Initial options whose typed execution representation must not replace the
/// original wire spelling when a session entry is written.
pub const INITIAL_SNAPSHOT_WIRE_OPTIONS: &[&str] = &["min-split-size"];

/// Initial options consumed by task creation rather than download behavior.
///
/// `gid` is an identity allocator input and must never be serialized as a
/// normal per-download behavior option.
pub const INITIAL_IDENTITY_OPTIONS: &[&str] = &["gid"];

/// Returns whether an option belongs to a request-group's initial state.
pub fn is_initial_option(option_name: &str) -> bool {
    INITIAL_REQUEST_OPTIONS.contains(&option_name)
}

/// Keep only the options that may be stored and reported by a request group.
///
/// The input is intentionally owned so each adapter can pass its raw option
/// state without exposing a second allowlist or accidentally retaining
/// process-only/session metadata in a task snapshot.
pub fn project_initial_options<I>(options: I) -> HashMap<String, serde_json::Value>
where
    I: IntoIterator<Item = (String, serde_json::Value)>,
{
    let mut projected = HashMap::new();
    for (name, value) in options {
        let canonical_name = crate::config::OptionRegistry::canonical_name(&name).to_string();
        if !is_initial_option(&canonical_name) {
            continue;
        }
        if name == canonical_name || !projected.contains_key(&canonical_name) {
            projected.insert(canonical_name, value);
        }
    }
    projected
}

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
    #[cfg(feature = "bittorrent")]
    "enable-public-trackers",
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
        is_global_option_changeable, is_initial_option, is_option_changeable,
        project_initial_options,
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
    fn initial_option_projection_keeps_request_options_only() {
        let projected = project_initial_options(std::collections::HashMap::from([
            ("dir".to_string(), serde_json::json!("/downloads")),
            ("rpc-secret".to_string(), serde_json::json!("secret")),
            ("enable-rpc".to_string(), serde_json::json!("true")),
            (
                "aria2-rust-metadata-uri".to_string(),
                serde_json::json!("https://example.test/metadata.torrent"),
            ),
        ]));

        assert_eq!(projected.get("dir"), Some(&serde_json::json!("/downloads")));
        assert!(!projected.contains_key("rpc-secret"));
        assert!(!projected.contains_key("enable-rpc"));
        assert!(!projected.contains_key("aria2-rust-metadata-uri"));
        assert!(is_initial_option("dir"));
        assert!(!is_initial_option("rpc-secret"));
    }

    #[test]
    fn task_policy_matches_original_changeability_axes() {
        assert_eq!(RUNTIME_CHANGEABLE_OPTIONS.len(), 7);
        #[cfg(feature = "bittorrent")]
        assert_eq!(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.len(), 106);
        #[cfg(not(feature = "bittorrent"))]
        assert_eq!(RUNTIME_CHANGEABLE_FOR_RESERVED_OPTIONS.len(), 105);
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
