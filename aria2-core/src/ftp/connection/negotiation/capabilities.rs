//! FTP server capabilities detection and tracking.
//!
//! Implements FEAT command parsing per RFC 2389 and tracks which features
//! the remote server supports. This drives decisions like:
//! - Whether to send `OPTS UTF8 ON` after detecting UTF8 capability
//! - Whether MLST/MLSD can be used for directory listings
//! - Whether SIZE/MDTM are safe to rely on
//! - Whether EPSV/EPRT extensions are available
//! - Whether TVFS (trivial virtual file store) paths are in use

use tracing::{debug, info, warn};

use crate::error::Result;
use crate::ftp::connection::negotiation::control::FreshControl;

// =============================================================================
// ServerCapabilities - tracks what the remote FTP server supports
// =============================================================================

/// Detected capabilities of an FTP server, populated from FEAT response.
///
/// Per RFC 2389, the FEAT command returns a multi-line 211 response where
/// each feature line is prefixed with a space. We parse these lines to
/// detect which extensions the server advertises.
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    /// Server supports UTF8 path encoding (RFC 2640)
    pub utf8: bool,
    /// Server supports MLST/MLSD machine-readable listings (RFC 3659)
    pub mlst_mlsd: bool,
    /// Server supports SIZE command (RFC 3659)
    pub size: bool,
    /// Server supports MDTM command (RFC 3659)
    pub mdtm: bool,
    /// Server supports EPSV extended passive mode (RFC 2428)
    pub epsv: bool,
    /// Server supports EPRT extended active mode (RFC 2428)
    pub eprt: bool,
    /// Server supports TVFS (trivial virtual file store, RFC 3659)
    pub tvfs: bool,
    /// System type from SYST command (e.g., "UNIX", "VMS", "Windows_NT")
    pub syst: Option<String>,
}

impl ServerCapabilities {
    /// Create a new capability set with all features disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a FEAT multi-line response body and populate capabilities.
    ///
    /// The FEAT response format per RFC 2389:
    /// ```text
    /// 211-Features:
    ///  UTF8
    ///  MLST type*;size*;modify*;
    ///  SIZE
    ///  MDTM
    ///  EPSV
    ///  EPRT
    ///  TVFS
    /// 211 End
    /// ```
    ///
    /// Each feature line starts with a space followed by the feature keyword.
    /// Lines without a leading space are not feature lines.
    pub fn parse_feat_response(&mut self, response_body: &str) {
        for line in response_body.lines() {
            // Feature lines start with a space per RFC 2389
            let feature = if line.starts_with(' ') {
                line.trim()
            } else {
                continue;
            };

            if feature.is_empty() {
                continue;
            }

            // Extract the feature keyword (first word before any space or semicolon).
            // Per RFC 2389, a feature line like "MLST type*;size*;modify*;" has
            // "MLST" as the keyword. We must split on whitespace first, then on
            // semicolons to avoid "MLST type*" being treated as the keyword.
            let first_token = feature
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or(feature)
                .trim();
            let keyword = first_token;

            self.apply_feature(keyword, feature);
        }

        info!(
            "FEAT parsed: utf8={}, mlst_mlsd={}, size={}, mdtm={}, epsv={}, eprt={}, tvfs={}",
            self.utf8, self.mlst_mlsd, self.size, self.mdtm, self.epsv, self.eprt, self.tvfs
        );
    }

    /// Apply a single feature keyword to the capability set.
    fn apply_feature(&mut self, keyword: &str, _full_line: &str) {
        // Feature keywords are case-insensitive per RFC 2389
        match keyword.to_uppercase().as_str() {
            "UTF8" => {
                self.utf8 = true;
                debug!("FEAT: UTF8 support detected");
            }
            "MLST" | "MLSD" => {
                self.mlst_mlsd = true;
                debug!("FEAT: MLST/MLSD support detected");
            }
            "SIZE" => {
                self.size = true;
                debug!("FEAT: SIZE support detected");
            }
            "MDTM" => {
                self.mdtm = true;
                debug!("FEAT: MDTM support detected");
            }
            "EPSV" => {
                self.epsv = true;
                debug!("FEAT: EPSV support detected");
            }
            "EPRT" => {
                self.eprt = true;
                debug!("FEAT: EPRT support detected");
            }
            "TVFS" => {
                self.tvfs = true;
                debug!("FEAT: TVFS support detected");
            }
            other => {
                debug!("FEAT: unknown feature '{}'", other);
            }
        }
    }
}

// =============================================================================
// FEAT command helpers
// =============================================================================

/// Send FEAT command and parse the response into a `ServerCapabilities`.
///
/// If the server does not support FEAT (returns an error code), returns
/// an empty capability set with all features disabled.
pub async fn query_feat(ctrl: &mut FreshControl) -> Result<ServerCapabilities> {
    debug!("Sending FEAT command");
    let resp = ctrl.command("FEAT").await?;

    if resp.0 != 211 {
        info!(
            "FEAT command not supported (response: {} {}), using defaults",
            resp.0, resp.1
        );
        return Ok(ServerCapabilities::new());
    }

    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(&resp.1);
    Ok(caps)
}

/// Send OPTS UTF8 ON to enable UTF-8 path encoding.
///
/// Per RFC 2640, this should be sent after FEAT indicates UTF8 support.
/// A 200 response means the server accepted the option. A non-200 response
/// is not fatal; we simply continue with the default encoding.
pub async fn send_opts_utf8_on(ctrl: &mut FreshControl) -> Result<bool> {
    debug!("Sending OPTS UTF8 ON");
    let resp = ctrl.command("OPTS UTF8 ON").await?;

    if (200..300).contains(&resp.0) {
        info!("UTF-8 path encoding enabled (OPTS UTF8 ON accepted)");
        Ok(true)
    } else {
        warn!(
            "OPTS UTF8 ON rejected ({} {}), continuing with default encoding",
            resp.0, resp.1
        );
        Ok(false)
    }
}

/// Send SYST command to query the server system type.
///
/// The response is a 215 code followed by the system name (e.g., "UNIX",
/// "VMS", "Windows_NT"). This is used in C++ aria2 for VMS-specific path
/// handling where directory separators differ.
pub async fn query_syst(ctrl: &mut FreshControl) -> Result<Option<String>> {
    debug!("Sending SYST command");
    let resp = ctrl.command("SYST").await?;

    if resp.0 != 215 {
        info!("SYST command returned non-215: {} {}", resp.0, resp.1);
        return Ok(None);
    }

    // Parse system type from "215 SystemType" response
    let msg = resp.1.trim();
    let systype = if let Some(stripped) = msg.strip_prefix("215") {
        stripped.trim().to_string()
    } else {
        msg.to_string()
    };

    info!("Server system type: {}", systype);
    Ok(Some(systype))
}

// =============================================================================
// Additional FTP commands
// =============================================================================

/// Send CDUP to change to the parent directory.
///
/// Useful for relative path traversal when the server uses non-standard
/// directory structures. Returns Ok(true) on success (200/250 response).
#[allow(dead_code)] // Will be used by higher-level download orchestrator
pub async fn send_cdup(ctrl: &mut FreshControl) -> Result<bool> {
    debug!("Sending CDUP command");
    let resp = ctrl.command("CDUP").await?;

    if (200..300).contains(&resp.0) {
        debug!("CDUP successful");
        Ok(true)
    } else {
        warn!("CDUP failed: {} {}", resp.0, resp.1);
        Ok(false)
    }
}

/// Send MKD to create a directory on the server.
///
/// Returns `Ok(true)` if the directory was created (257 response).
/// Returns `Ok(false)` for non-fatal failures (directory may already exist).
/// Returns an error for permanent failures.
#[allow(dead_code)] // Will be used by higher-level download orchestrator
pub async fn send_mkd(ctrl: &mut FreshControl, path: &str) -> Result<bool> {
    debug!("Sending MKD command for: {}", path);
    let resp = ctrl.command(&format!("MKD {}", path)).await?;

    if resp.0 == 257 {
        debug!("MKD successful: {}", path);
        Ok(true)
    } else if resp.0 == 550 {
        // Directory already exists or path unavailable - not a fatal error
        debug!("MKD returned 550 (directory may already exist): {}", path);
        Ok(false)
    } else {
        warn!("MKD failed for {}: {} {}", path, resp.0, resp.1);
        Ok(false)
    }
}

/// Send NOOP as a keep-alive probe for long idle connections.
///
/// A 200 response confirms the control connection is still alive.
/// Returns `Ok(true)` if the connection is alive, `Ok(false)` otherwise.
#[allow(dead_code)] // Will be used by connection keep-alive logic
pub async fn send_noop(ctrl: &mut FreshControl) -> Result<bool> {
    debug!("Sending NOOP command");
    let resp = ctrl.command("NOOP").await?;

    if resp.0 == 200 {
        debug!("NOOP successful, connection alive");
        Ok(true)
    } else {
        warn!("NOOP unexpected response: {} {}", resp.0, resp.1);
        Ok(false)
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_feat_response_full() {
        let response = "\
211-Features:
 UTF8
 MLST type*;size*;modify*;
 SIZE
 MDTM
 EPSV
 EPRT
 TVFS
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.utf8);
        assert!(caps.mlst_mlsd);
        assert!(caps.size);
        assert!(caps.mdtm);
        assert!(caps.epsv);
        assert!(caps.eprt);
        assert!(caps.tvfs);
    }

    #[test]
    fn test_parse_feat_response_minimal() {
        // Minimal FEAT response with only UTF8
        let response = "\
211-Features:
 UTF8
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.utf8);
        assert!(!caps.mlst_mlsd);
        assert!(!caps.size);
        assert!(!caps.mdtm);
    }

    #[test]
    fn test_parse_feat_response_empty() {
        let response = "211 End";
        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(!caps.utf8);
        assert!(!caps.mlst_mlsd);
    }

    #[test]
    fn test_parse_feat_response_case_insensitive() {
        let response = "\
211-Features:
 utf8
 Mlst type*;size*;
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.utf8);
        assert!(caps.mlst_mlsd);
    }

    #[test]
    fn test_parse_feat_mlst_with_semicolons() {
        // MLST lines can contain semicolons with type info
        let response = "\
211-Features:
 MLST type*;size*;modify*;perm*;
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.mlst_mlsd);
    }

    #[test]
    fn test_parse_feat_unknown_feature() {
        let response = "\
211-Features:
 UTF8
 SOME_UNKNOWN_FEATURE
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.utf8);
        // Unknown features are silently ignored
    }

    #[test]
    fn test_server_capabilities_default() {
        let caps = ServerCapabilities::new();
        assert!(!caps.utf8);
        assert!(!caps.mlst_mlsd);
        assert!(!caps.size);
        assert!(!caps.mdtm);
        assert!(!caps.epsv);
        assert!(!caps.eprt);
        assert!(!caps.tvfs);
        assert!(caps.syst.is_none());
    }

    #[test]
    fn test_parse_feat_mlsd_separate() {
        // Some servers list MLSD as a separate feature
        let response = "\
211-Features:
 MLST type*;size*;
 MLSD
211 End";

        let mut caps = ServerCapabilities::new();
        caps.parse_feat_response(response);

        assert!(caps.mlst_mlsd);
    }
}
