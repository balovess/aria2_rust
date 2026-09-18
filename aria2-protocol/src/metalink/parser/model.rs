use super::super::resource::{LOWEST_PRIORITY, ResourceType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetalinkVersion {
    V3,
    V4,
}

impl MetalinkVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::V3 => "V3",
            Self::V4 => "V4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgorithm {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "md5" | "md5sum" => Some(Self::Md5),
            "sha-1" | "sha1" | "sha1sum" => Some(Self::Sha1),
            "sha-224" | "sha224" | "sha224sum" => Some(Self::Sha224),
            "sha-256" | "sha256" | "sha256sum" => Some(Self::Sha256),
            "sha-384" | "sha384" | "sha384sum" => Some(Self::Sha384),
            "sha-512" | "sha512" | "sha512sum" => Some(Self::Sha512),
            _ => None,
        }
    }

    pub fn hash_len(&self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha224 => 56,
            Self::Sha256 => 64,
            Self::Sha384 => 96,
            Self::Sha512 => 128,
        }
    }

    pub fn as_standard_name(&self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha-1",
            Self::Sha224 => "sha-224",
            Self::Sha256 => "sha-256",
            Self::Sha384 => "sha-384",
            Self::Sha512 => "sha-512",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HashEntry {
    pub algo: HashAlgorithm,
    pub value: String,
}

impl HashEntry {
    pub fn new(algo: HashAlgorithm, value: &str) -> Self {
        Self {
            algo,
            value: value.trim().to_lowercase(),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.value.len() == self.algo.hash_len()
    }
}

#[derive(Debug, Clone)]
pub struct UrlEntry {
    pub url: String,
    pub priority: i32,
    pub location: Option<String>,
    pub max_connections: Option<u32>,
    pub preference: Option<i32>,
    /// Protocol type of this resource.
    ///
    /// Mirrors C++ `MetalinkResource::type`. Auto-detected from URL scheme
    /// on construction; overridden by V3 `<url type="http">` attributes.
    pub resource_type: ResourceType,
}

impl UrlEntry {
    pub fn new(url: &str) -> Self {
        let url_trimmed = url.trim().to_string();
        let resource_type = ResourceType::from_url(&url_trimmed);
        Self {
            url: url_trimmed,
            priority: LOWEST_PRIORITY,
            location: None,
            max_connections: None,
            preference: None,
            resource_type,
        }
    }

    pub fn with_priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }
    pub fn with_location(mut self, loc: &str) -> Self {
        self.location = Some(loc.to_string());
        self
    }
    pub fn with_max_connections(mut self, n: u32) -> Self {
        self.max_connections = Some(n);
        self
    }
    pub fn with_preference(mut self, p: i32) -> Self {
        self.preference = Some(p);
        self
    }
    pub fn with_resource_type(mut self, rt: ResourceType) -> Self {
        self.resource_type = rt;
        self
    }

    /// Whether this URL is a non-P2P type (HTTP, HTTPS, FTP).
    /// Mirrors C++ `AccumulateNonP2PUri` filter.
    pub fn is_non_p2p(&self) -> bool {
        self.resource_type.is_non_p2p()
    }

    /// Whether this URL's protocol is supported for downloading.
    /// Mirrors C++ `MetalinkEntry::dropUnsupportedResource()`.
    pub fn is_supported(&self) -> bool {
        self.resource_type.is_supported()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaType {
    Torrent,
    Xml,
    Other(String),
}

impl MediaType {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "application/x-bittorrent" | "torrent" => Self::Torrent,
            "application/xml" | "text/xml" | "xml" => Self::Xml,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_torrent(&self) -> bool {
        matches!(self, Self::Torrent)
    }
}

#[derive(Debug, Clone)]
pub struct MetaUrlEntry {
    pub url: String,
    pub mediatype: MediaType,
    pub priority: i32,
    pub name: Option<String>,
}

impl MetaUrlEntry {
    /// Default priority for unsorted/unspecified metaurl entries.
    /// Matches C++ `MetalinkResource::getLowestPriority()` = 999999.
    pub const LOWEST_PRIORITY: i32 = 999999;

    pub fn new(url: &str, mediatype: MediaType) -> Self {
        Self {
            url: url.trim().to_string(),
            mediatype,
            priority: Self::LOWEST_PRIORITY,
            name: None,
        }
    }

    pub fn with_priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }
    pub fn with_name(mut self, n: &str) -> Self {
        self.name = Some(n.to_string());
        self
    }
}

#[derive(Debug, Clone)]
pub struct PieceInfo {
    pub length: u32,
    pub type_: HashAlgorithm,
    pub hashes: Vec<String>,
}

impl PieceInfo {
    pub fn num_pieces(&self, file_size: u64) -> usize {
        if self.length == 0 || file_size == 0 {
            return 0;
        }
        file_size.div_ceil(self.length as u64) as usize
    }

    /// Number of piece hashes parsed so far.
    ///
    /// Each entry of `hashes` is one complete hex digest of one piece
    /// (mirroring C++ `ChunkChecksum::getPieceHashes()` where each element is
    /// a binary digest of one chunk). Previously this divided by the hex
    /// length, which was wrong for both supported encodings.
    pub fn piece_count(&self) -> usize {
        self.hashes.len()
    }
}

#[derive(Debug, Clone)]
pub struct MetalinkFile {
    pub name: String,
    pub size: Option<u64>,
    /// True if size was explicitly specified in Metalink document.
    /// Mirrors C++ `MetalinkEntry::sizeKnown`.
    pub size_known: bool,
    pub identity: Option<String>,
    /// Version string (V3/V4).
    /// Mirrors C++ `MetalinkEntry::version`.
    pub version: Option<String>,
    /// Language codes (V3/V4).
    /// Mirrors C++ `MetalinkEntry::languages`.
    pub languages: Vec<String>,
    /// Operating system codes (V3/V4).
    /// Mirrors C++ `MetalinkEntry::oses`.
    pub oses: Vec<String>,
    pub hashes: Vec<HashEntry>,
    pub urls: Vec<UrlEntry>,
    pub meta_urls: Vec<MetaUrlEntry>,
    pub pieces: Option<PieceInfo>,
    /// Maximum connections per server (V3 only).
    /// Mirrors C++ `MetalinkEntry::maxConnections`.
    pub max_connections: Option<i32>,
}

impl MetalinkFile {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            size: None,
            size_known: false,
            identity: None,
            version: None,
            languages: Vec::new(),
            oses: Vec::new(),
            hashes: Vec::new(),
            urls: Vec::new(),
            meta_urls: Vec::new(),
            pieces: None,
            max_connections: None,
        }
    }

    pub fn get_preferred_url(&self) -> Option<&UrlEntry> {
        self.urls.iter().min_by_key(|url| url.priority)
    }

    pub fn get_sorted_urls(&self) -> Vec<&UrlEntry> {
        let mut sorted: Vec<&UrlEntry> = self.urls.iter().collect();
        sorted.sort_by_key(|a| a.priority);
        sorted
    }

    pub fn get_hash(&self, algo: HashAlgorithm) -> Option<&HashEntry> {
        self.hashes.iter().find(|h| h.algo == algo)
    }
    pub fn has_torrent_metaurl(&self) -> bool {
        self.meta_urls.iter().any(|m| m.mediatype.is_torrent())
    }
    pub fn total_size(&self) -> Option<u64> {
        self.size
    }

    /// Return the strongest available hash entry.
    ///
    /// Implements the "strongest hash wins" logic from C++
    /// `MetalinkParserController.cc:308-314` where SHA-512 > SHA-256 >
    /// SHA-1 > MD5. When multiple hashes of the same algorithm exist,
    /// the first one is returned.
    pub fn strongest_hash(&self) -> Option<&HashEntry> {
        const PRIORITY: &[HashAlgorithm] = &[
            HashAlgorithm::Sha512,
            HashAlgorithm::Sha384,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha224,
            HashAlgorithm::Sha1,
            HashAlgorithm::Md5,
        ];
        for algo in PRIORITY {
            if let Some(entry) = self.get_hash(*algo) {
                return Some(entry);
            }
        }
        self.hashes.first()
    }

    /// Check if this entry contains a given language code.
    ///
    /// Mirrors C++ `MetalinkEntry::containsLanguage()`.
    pub fn contains_language(&self, lang: &str) -> bool {
        self.languages.iter().any(|l| l.eq_ignore_ascii_case(lang))
    }

    /// Check if this entry supports a given OS.
    ///
    /// Mirrors C++ `MetalinkEntry::containsOS()`.
    pub fn contains_os(&self, os: &str) -> bool {
        self.oses.iter().any(|o| o.eq_ignore_ascii_case(os))
    }

    /// Remove URLs whose resource type is not supported for downloading.
    ///
    /// Mirrors C++ `MetalinkEntry::dropUnsupportedResource()` which
    /// erases resources whose type is not FTP, HTTP, HTTPS, or BitTorrent.
    /// In this Rust port we treat HTTPS and BitTorrent as always supported
    /// (the C++ code gates them behind `ENABLE_SSL` / `ENABLE_BITTORRENT`
    /// compile-time flags which are always enabled in our build).
    ///
    /// Both `NotSupported` and `Unknown` types are removed, matching C++
    /// where the `default` case in the switch covers all non-FTP/HTTP/HTTPS/BT types.
    pub fn drop_unsupported_resources(&mut self) {
        self.urls.retain(|url| url.resource_type.is_supported());
    }

    /// Add `priority_to_add` to URLs whose location matches one of the given
    /// location codes.
    ///
    /// Mirrors C++ `MetalinkEntry::setLocationPriority()`:
    /// ```cpp
    /// for (auto& res : resources) {
    ///   if (std::find(locations.begin(), locations.end(), res->location)
    ///       != locations.end()) {
    ///     res->priority += priorityToAdd;
    ///   }
    /// }
    /// ```
    pub fn set_location_priority(&mut self, locations: &[&str], priority_to_add: i32) {
        for url in &mut self.urls {
            if let Some(ref loc) = url.location
                && locations.iter().any(|l| l.eq_ignore_ascii_case(loc))
            {
                url.priority += priority_to_add;
            }
        }
    }

    /// Add `priority_to_add` to URLs whose resource type string matches the
    /// given protocol name (e.g. `"http"`, `"https"`, `"ftp"`).
    ///
    /// Mirrors C++ `MetalinkEntry::setProtocolPriority()`:
    /// ```cpp
    /// for (auto& res : resources) {
    ///   if (protocol == MetalinkResource::getTypeString(res->type)) {
    ///     res->priority += priorityToAdd;
    ///   }
    /// }
    /// ```
    pub fn set_protocol_priority(&mut self, protocol: &str, priority_to_add: i32) {
        for url in &mut self.urls {
            if url.resource_type.as_str().eq_ignore_ascii_case(protocol) {
                url.priority += priority_to_add;
            }
        }
    }

    /// Shuffle URLs randomly, then sort by priority ascending.
    ///
    /// Mirrors C++ `MetalinkEntry::reorderResourcesByPriority()`:
    /// ```cpp
    /// std::shuffle(resources.begin(), resources.end(), rng);
    /// std::sort(resources.begin(), resources.end(), PriorityHigher{});
    /// ```
    /// The shuffle ensures that URLs with equal priority are tried in random
    /// order (load-balancing across mirrors), while the sort guarantees lower
    /// priority values are tried first.
    pub fn reorder_resources_by_priority(&mut self) {
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();
        self.urls.shuffle(&mut rng);
        self.urls.sort_by_key(|u| u.priority);
    }

    /// Sort metaurls by priority ascending.
    ///
    /// Mirrors C++ `MetalinkEntry::reorderMetaurlsByPriority()`:
    /// ```cpp
    /// std::sort(metaurls.begin(), metaurls.end(), PriorityHigher{});
    /// ```
    /// Unlike `reorder_resources_by_priority()`, metaurls are NOT shuffled
    /// before sorting — deterministic order within equal priority.
    pub fn reorder_metaurls_by_priority(&mut self) {
        self.meta_urls.sort_by_key(|m| m.priority);
    }
}

#[derive(Debug, Clone)]
pub struct MetalinkDocument {
    pub version: MetalinkVersion,
    pub files: Vec<MetalinkFile>,
    pub generator: Option<String>,
    pub origin: Option<String>,
    pub published: Option<String>,
    /// Base URI for resolving relative URLs found in this document.
    /// Mirrors C++ `MetalinkParserController::baseUri_`.
    pub base_uri: Option<String>,
}
