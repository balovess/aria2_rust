//! Metalink Resource — represents a download source (mirror) for a Metalink file.
//!
//! Each Metalink file can have multiple resources (mirrors) with different
//! priorities, preferences, and location constraints.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `MetalinkResource` | `MetalinkResource` |
//! | `ResourceType` | `MetalinkResource::TYPE` |

/// Protocol type of a Metalink resource URL.
///
/// Mirrors C++ `MetalinkResource::TYPE` enum values.
/// Used for filtering unsupported protocols and for protocol-priority boosting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    /// FTP protocol
    Ftp,
    /// HTTP protocol
    Http,
    /// HTTPS protocol
    Https,
    /// BitTorrent (magnet or .torrent) — requires BT feature flag
    BitTorrent,
    /// Protocol not supported by this build
    NotSupported,
    /// Could not determine the protocol
    Unknown,
}

impl ResourceType {
    /// Return the lowercase protocol string used for comparison.
    ///
    /// Mirrors C++ `MetalinkResource::type2String[]`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ftp => "ftp",
            Self::Http => "http",
            Self::Https => "https",
            Self::BitTorrent => "bittorrent",
            Self::NotSupported => "not_supported",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a URL into its resource type.
    ///
    /// Mirrors the logic in C++ `MetalinkParserStateV3Impl.cc` and
    /// `MetalinkParserStateV4Impl.cc` where the URL type is determined
    /// from the `type` attribute (V3) or from the URL scheme (V4).
    pub fn from_url(url: &str) -> Self {
        let lower = url.to_lowercase();
        if lower.starts_with("http://") {
            Self::Http
        } else if lower.starts_with("https://") {
            Self::Https
        } else if lower.starts_with("ftp://") {
            Self::Ftp
        } else if lower.starts_with("magnet:") || lower.ends_with(".torrent") {
            Self::BitTorrent
        } else {
            Self::Unknown
        }
    }

    /// Classify from a V3 `type` attribute value.
    ///
    /// In Metalink V3, each `<url>` element has a `type` attribute
    /// ("http", "https", "ftp", "bittorrent"). Unrecognized types are
    /// classified as `NotSupported`, matching C++ `MetalinkParserController::setTypeOfResource()`.
    pub fn from_v3_type(type_attr: &str) -> Self {
        match type_attr.to_lowercase().as_str() {
            "ftp" | "sftp" => Self::Ftp,
            "http" => Self::Http,
            "https" => Self::Https,
            "bittorrent" | "torrent" => Self::BitTorrent,
            _ => Self::NotSupported,
        }
    }

    /// Alias for [`from_v3_type`] used by the Metalink parser.
    ///
    /// The parser calls `ResourceType::from_url_type_str(val)` to match
    /// the V3 `<url type="...">` attribute.
    pub fn from_url_type_str(type_attr: &str) -> Self {
        Self::from_v3_type(type_attr)
    }

    /// Whether this resource type is supported for downloading.
    ///
    /// Mirrors C++ `MetalinkEntry::dropUnsupportedResource()` — only
    /// HTTP, HTTPS, FTP, and BitTorrent are kept; the rest are dropped.
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Ftp | Self::Http | Self::Https | Self::BitTorrent)
    }

    /// Whether this is a non-P2P resource type (HTTP, HTTPS, FTP).
    ///
    /// Mirrors C++ `AccumulateNonP2PUri` which only collects HTTP/HTTPS/FTP
    /// URLs for the URI list (BitTorrent goes through a separate dependency
    /// mechanism in `Metalink2RequestGroup`).
    pub fn is_non_p2p(&self) -> bool {
        matches!(self, Self::Ftp | Self::Http | Self::Https)
    }
}

/// A download resource (mirror) for a Metalink file entry.
///
/// Contains the URL along with priority, preference, and geo-location
/// metadata used to select the best mirror.
///
/// Mirrors C++ `MetalinkResource`.
#[derive(Debug, Clone)]
pub struct MetalinkResource {
    /// The URL of this resource
    pub url: String,
    /// Protocol type of this resource
    pub resource_type: ResourceType,
    /// Priority (lower = preferred, per Metalink spec)
    pub priority: i32,
    /// Preference value (higher = preferred, aria2 extension)
    pub preference: i32,
    /// Geographic location constraint (ISO 3166-1 alpha-2 country code)
    pub location: String,
    /// Maximum number of concurrent connections to this resource
    pub max_connections: i32,
}

/// Default lowest priority for unsorted resources.
///
/// Mirrors C++ `MetalinkResource::getLowestPriority()` = 999999.
pub const LOWEST_PRIORITY: i32 = 999999;

impl MetalinkResource {
    /// Create a new resource with the given URL and default priority.
    pub fn new(url: impl Into<String>) -> Self {
        let url_str = url.into();
        let resource_type = ResourceType::from_url(&url_str);
        Self {
            url: url_str,
            resource_type,
            priority: LOWEST_PRIORITY,
            preference: 0,
            location: String::new(),
            max_connections: -1,
        }
    }

    /// Create a new resource with explicit priority.
    pub fn with_priority(url: impl Into<String>, priority: i32) -> Self {
        Self {
            priority,
            ..Self::new(url)
        }
    }

    /// Check if this resource is preferred over another.
    ///
    /// Lower priority value wins. Ties are broken by higher preference.
    pub fn is_preferred_over(&self, other: &MetalinkResource) -> bool {
        if self.priority != other.priority {
            self.priority < other.priority
        } else {
            self.preference > other.preference
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_type_from_url() {
        assert_eq!(ResourceType::from_url("http://example.com/file"), ResourceType::Http);
        assert_eq!(ResourceType::from_url("https://example.com/file"), ResourceType::Https);
        assert_eq!(ResourceType::from_url("ftp://example.com/file"), ResourceType::Ftp);
        assert_eq!(ResourceType::from_url("magnet:?xt=urn:btih:abc"), ResourceType::BitTorrent);
        assert_eq!(ResourceType::from_url("file.torrent"), ResourceType::BitTorrent);
        assert_eq!(ResourceType::from_url("unknown://host"), ResourceType::Unknown);
    }

    #[test]
    fn test_resource_type_from_v3_type() {
        assert_eq!(ResourceType::from_v3_type("http"), ResourceType::Http);
        assert_eq!(ResourceType::from_v3_type("HTTPS"), ResourceType::Https);
        assert_eq!(ResourceType::from_v3_type("ftp"), ResourceType::Ftp);
        assert_eq!(ResourceType::from_v3_type("bittorrent"), ResourceType::BitTorrent);
        assert_eq!(ResourceType::from_v3_type("unknown"), ResourceType::NotSupported);
    }

    #[test]
    fn test_resource_type_is_supported() {
        assert!(ResourceType::Http.is_supported());
        assert!(ResourceType::Https.is_supported());
        assert!(ResourceType::Ftp.is_supported());
        assert!(ResourceType::BitTorrent.is_supported());
        assert!(!ResourceType::NotSupported.is_supported());
        assert!(!ResourceType::Unknown.is_supported());
    }

    #[test]
    fn test_resource_type_is_non_p2p() {
        assert!(ResourceType::Http.is_non_p2p());
        assert!(ResourceType::Https.is_non_p2p());
        assert!(ResourceType::Ftp.is_non_p2p());
        assert!(!ResourceType::BitTorrent.is_non_p2p());
    }

    #[test]
    fn test_lowest_priority() {
        assert_eq!(LOWEST_PRIORITY, 999999);
    }

    #[test]
    fn test_new_resource_defaults() {
        let res = MetalinkResource::new("http://example.com/file");
        assert_eq!(res.resource_type, ResourceType::Http);
        assert_eq!(res.priority, LOWEST_PRIORITY);
        assert_eq!(res.preference, 0);
        assert_eq!(res.max_connections, -1);
        assert!(res.location.is_empty());
    }
}
