//! FTP feature negotiation and extended commands.
//!
//! Contains the `FtpFeatures` struct (parsed from FEAT response per RFC 2389)
//! and `FtpClient` methods for FEAT, CDUP, MKD, OPTS UTF8 ON, and AUTH TLS.

use tracing::{debug, info, warn};

use crate::error::Result;

use super::types::{FtpClient, FtpResponse};

// =============================================================================
// FtpFeatures - parsed from FEAT multi-line response (RFC 2389)
// =============================================================================

/// Parsed features from FEAT command response (RFC 2389).
///
/// Populated by `FtpClient::send_feat()` from the multi-line 211 response.
/// Each boolean flag indicates whether the server advertised that feature.
/// The `features` field holds raw feature strings for features not covered
/// by the explicit flags.
#[derive(Debug, Clone, Default)]
pub struct FtpFeatures {
    /// Server supports UTF8 path encoding (RFC 2640)
    pub utf8: bool,
    /// Server supports MLSD machine-readable directory listings (RFC 3659)
    pub mlsd: bool,
    /// Server supports MLST machine-readable file metadata (RFC 3659)
    pub mlst: bool,
    /// Server supports PRET (pre-transfer) command (distributed FTP)
    pub pret: bool,
    /// Server supports MFMT modification time command (RFC 3659)
    pub mfmt: bool,
    /// Server supports TVFS trivial virtual file store (RFC 3659)
    pub tvfs: bool,
    /// Server supports AUTH TLS (RFC 4217)
    pub auth_tls: bool,
    /// Server supports AUTH SSL
    pub auth_ssl: bool,
    /// Server supports EPSV extended passive mode (RFC 2428)
    pub epsv: bool,
    /// Server supports EPRT extended active mode (RFC 2428)
    pub eprt: bool,
    /// Server supports SIZE command (RFC 3659)
    pub size: bool,
    /// Server supports REST STREAM (stream-level resume, RFC 3659)
    pub rest_stem: bool,
    /// Raw feature strings not matched by the explicit flags above
    pub features: Vec<String>,
}

impl FtpFeatures {
    /// Parse the body of a 211 FEAT response into `FtpFeatures`.
    ///
    /// Per RFC 2389, each feature line is prefixed with a space.
    /// Lines without a leading space are not feature lines and are skipped.
    /// Feature keywords are case-insensitive.
    pub fn parse(body: &str) -> Self {
        let mut feat = Self::default();

        for line in body.lines() {
            // Feature lines start with a space per RFC 2389
            let feature = if line.starts_with(' ') {
                line.trim()
            } else {
                continue;
            };

            if feature.is_empty() {
                continue;
            }

            // Extract keyword: first token before whitespace or semicolon.
            // E.g. "MLST type*;size*;modify*;" -> keyword is "MLST"
            let keyword = feature
                .split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or(feature)
                .trim();

            feat.apply_keyword(keyword, feature);
        }

        info!(
            "FEAT parsed: utf8={}, mlsd={}, mlst={}, pret={}, mfmt={}, tvfs={}, \
             auth_tls={}, auth_ssl={}, epsv={}, eprt={}, size={}, rest_stem={}",
            feat.utf8,
            feat.mlsd,
            feat.mlst,
            feat.pret,
            feat.mfmt,
            feat.tvfs,
            feat.auth_tls,
            feat.auth_ssl,
            feat.epsv,
            feat.eprt,
            feat.size,
            feat.rest_stem,
        );

        feat
    }

    /// Apply a single feature keyword to the capability set.
    fn apply_keyword(&mut self, keyword: &str, full_line: &str) {
        // Feature keywords are case-insensitive per RFC 2389
        match keyword.to_uppercase().as_str() {
            "UTF8" => {
                self.utf8 = true;
                debug!("FEAT: UTF8 support detected");
            }
            "MLSD" => {
                self.mlsd = true;
                debug!("FEAT: MLSD support detected");
            }
            "MLST" => {
                self.mlst = true;
                debug!("FEAT: MLST support detected");
            }
            "PRET" => {
                self.pret = true;
                debug!("FEAT: PRET support detected");
            }
            "MFMT" => {
                self.mfmt = true;
                debug!("FEAT: MFMT support detected");
            }
            "TVFS" => {
                self.tvfs = true;
                debug!("FEAT: TVFS support detected");
            }
            "AUTH" => {
                // AUTH TLS vs AUTH SSL: check the full line
                let upper = full_line.to_uppercase();
                if upper.contains("TLS") {
                    self.auth_tls = true;
                    debug!("FEAT: AUTH TLS support detected");
                }
                if upper.contains("SSL") {
                    self.auth_ssl = true;
                    debug!("FEAT: AUTH SSL support detected");
                }
                // If neither TLS nor SSL is mentioned, record as generic AUTH
                if !upper.contains("TLS") && !upper.contains("SSL") {
                    debug!(
                        "FEAT: AUTH support detected (type unspecified): {}",
                        full_line
                    );
                }
            }
            "EPSV" => {
                self.epsv = true;
                debug!("FEAT: EPSV support detected");
            }
            "EPRT" => {
                self.eprt = true;
                debug!("FEAT: EPRT support detected");
            }
            "SIZE" => {
                self.size = true;
                debug!("FEAT: SIZE support detected");
            }
            "REST" => {
                // REST STREAM, REST PIPE, etc.
                let upper = full_line.to_uppercase();
                if upper.contains("STREAM") {
                    self.rest_stem = true;
                    debug!("FEAT: REST STREAM support detected");
                } else {
                    debug!("FEAT: REST support detected (variant: {})", full_line);
                }
            }
            _ => {
                debug!("FEAT: unknown feature '{}'", keyword);
                self.features.push(full_line.to_string());
            }
        }
    }
}

// =============================================================================
// FtpClient command implementations
// =============================================================================

impl FtpClient {
    /// Send FEAT command and parse server feature list (RFC 2389).
    ///
    /// The server responds with a multi-line 211 reply listing supported
    /// features. If the server does not support FEAT (returns a non-211
    /// response), returns `Ok(FtpFeatures::default())`.
    ///
    /// The parsed features are also stored in `self.features` for later access.
    pub async fn send_feat(&mut self) -> Result<FtpFeatures> {
        debug!("Sending FEAT command");
        self.send_command("FEAT").await?;
        let resp = self.read_response().await?;

        if resp.code != 211 {
            info!(
                "FEAT not supported (response: {} {}), using defaults",
                resp.code, resp.message
            );
            let feat = FtpFeatures::default();
            self.features = Some(feat.clone());
            return Ok(feat);
        }

        let feat = FtpFeatures::parse(&resp.message);
        self.features = Some(feat.clone());
        Ok(feat)
    }

    /// Send CDUP command to change to the parent directory.
    ///
    /// Returns `Ok(FtpResponse)` on 200/250 success, or an error on failure.
    /// Non-fatal failures (e.g. already at root) may return a 5xx response
    /// which is still returned as `Ok` for the caller to decide.
    pub async fn send_cdup(&mut self) -> Result<FtpResponse> {
        debug!("Sending CDUP command");
        self.send_command("CDUP").await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            debug!("CDUP successful");
        } else {
            warn!("CDUP failed: {} {}", resp.code, resp.message);
        }

        Ok(resp)
    }

    /// Send MKD command to create a directory on the server.
    ///
    /// Returns the 257 response on success (directory created).
    /// Returns `RecoverableError::ServerError { code: 550 }` if the
    /// directory already exists or the path is unavailable.
    pub async fn send_mkd(&mut self, path: &str) -> Result<FtpResponse> {
        debug!("Sending MKD command for: {}", path);
        self.send_command(&format!("MKD {}", path)).await?;
        let resp = self.read_response().await?;

        if resp.code == 257 {
            debug!("MKD successful: {}", path);
        } else if resp.code == 550 {
            debug!("MKD returned 550 (directory may already exist): {}", path);
        } else {
            warn!("MKD failed for {}: {} {}", path, resp.code, resp.message);
        }

        Ok(resp)
    }

    /// Send OPTS UTF8 ON to enable UTF-8 path encoding (RFC 2640).
    ///
    /// Should be called after `send_feat()` indicates UTF8 support.
    /// A 200 response means the server accepted the option. A non-200
    /// response is not fatal; the method still returns `Ok(FtpResponse)`.
    pub async fn send_opts_utf8(&mut self) -> Result<FtpResponse> {
        debug!("Sending OPTS UTF8 ON");
        self.send_command("OPTS UTF8 ON").await?;
        let resp = self.read_response().await?;

        if resp.is_positive_completion() {
            info!("UTF-8 path encoding enabled (OPTS UTF8 ON accepted)");
        } else {
            warn!(
                "OPTS UTF8 ON rejected ({} {}), continuing with default encoding",
                resp.code, resp.message
            );
        }

        Ok(resp)
    }

    /// Send AUTH TLS to request TLS protection on the control connection.
    ///
    /// Per RFC 4217, a 234 response means the server is ready for TLS
    /// handshake. The caller is responsible for upgrading the connection
    /// to a TLS stream after receiving a 234 response.
    ///
    /// Returns `Ok(FtpResponse)` regardless of whether TLS was accepted;
    /// the caller should inspect `resp.code` to decide next steps.
    pub async fn send_auth_tls(&mut self) -> Result<FtpResponse> {
        debug!("Sending AUTH TLS command");
        self.send_command("AUTH TLS").await?;
        let resp = self.read_response().await?;

        match resp.code {
            234 => {
                info!("AUTH TLS accepted (234), connection ready for TLS upgrade");
            }
            _ => {
                info!(
                    "AUTH TLS not supported by server ({} {})",
                    resp.code, resp.message
                );
            }
        }

        Ok(resp)
    }

    /// Get the server features, if they have been queried via `send_feat()`.
    pub fn features(&self) -> Option<&FtpFeatures> {
        self.features.as_ref()
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_feat_full() {
        let response = "\
211-Features:
 UTF8
 MLST type*;size*;modify*;
 MLSD
 SIZE
 MDTM
 EPSV
 EPRT
 TVFS
 PRET
 MFMT
 REST STREAM
 AUTH TLS
211 End";

        let feat = FtpFeatures::parse(response);

        assert!(feat.utf8);
        assert!(feat.mlst);
        assert!(feat.mlsd);
        assert!(feat.size);
        assert!(feat.epsv);
        assert!(feat.eprt);
        assert!(feat.tvfs);
        assert!(feat.pret);
        assert!(feat.mfmt);
        assert!(feat.rest_stem);
        assert!(feat.auth_tls);
        assert!(!feat.auth_ssl);
    }

    #[test]
    fn test_parse_feat_minimal() {
        let response = "\
211-Features:
 UTF8
211 End";

        let feat = FtpFeatures::parse(response);
        assert!(feat.utf8);
        assert!(!feat.mlsd);
        assert!(!feat.size);
    }

    #[test]
    fn test_parse_feat_empty() {
        let response = "211 End";
        let feat = FtpFeatures::parse(response);
        assert!(!feat.utf8);
        assert!(!feat.mlst);
    }

    #[test]
    fn test_parse_feat_case_insensitive() {
        let response = "\
211-Features:
 utf8
 Mlst type*;size*;
211 End";

        let feat = FtpFeatures::parse(response);
        assert!(feat.utf8);
        assert!(feat.mlst);
    }

    #[test]
    fn test_parse_feat_auth_ssl() {
        let response = "\
211-Features:
 AUTH SSL
211 End";

        let feat = FtpFeatures::parse(response);
        assert!(feat.auth_ssl);
        assert!(!feat.auth_tls);
    }

    #[test]
    fn test_parse_feat_rest_stream() {
        let response = "\
211-Features:
 REST STREAM
211 End";

        let feat = FtpFeatures::parse(response);
        assert!(feat.rest_stem);
    }

    #[test]
    fn test_parse_feat_unknown_features_collected() {
        let response = "\
211-Features:
 UTF8
 XCURSOR
 SOME_UNKNOWN_FEATURE
211 End";

        let feat = FtpFeatures::parse(response);
        assert!(feat.utf8);
        // Unknown features should be collected in the features vec
        assert_eq!(feat.features.len(), 2);
    }

    #[test]
    fn test_feat_default() {
        let feat = FtpFeatures::default();
        assert!(!feat.utf8);
        assert!(!feat.mlsd);
        assert!(!feat.mlst);
        assert!(!feat.pret);
        assert!(!feat.mfmt);
        assert!(!feat.tvfs);
        assert!(!feat.auth_tls);
        assert!(!feat.auth_ssl);
        assert!(!feat.epsv);
        assert!(!feat.eprt);
        assert!(!feat.size);
        assert!(!feat.rest_stem);
        assert!(feat.features.is_empty());
    }
}
