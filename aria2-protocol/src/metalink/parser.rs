use super::resource::{LOWEST_PRIORITY, ResourceType};
use tracing::info;

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
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "md5" | "md5sum" => Some(Self::Md5),
            "sha-1" | "sha1" | "sha1sum" => Some(Self::Sha1),
            "sha-256" | "sha256" | "sha256sum" => Some(Self::Sha256),
            "sha-512" | "sha512" | "sha512sum" => Some(Self::Sha512),
            _ => None,
        }
    }

    pub fn hash_len(&self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    pub fn as_standard_name(&self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha-1",
            Self::Sha256 => "sha-256",
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

/// Split raw `<pieces>` character data into one complete hex digest per piece.
///
/// The Metalink v3-style document may write the digests either whitespace
/// separated or as one contiguous run (e.g. `hash1hash2`). C++ feeds each
/// digest individually through `MessageDigest::isValidHash`; we chunk by the
/// hex length of the algorithm and drop any trailing partial digest.
fn split_piece_hashes(text: &str, hex_len: usize) -> Vec<String> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .as_bytes()
        .chunks(hex_len)
        .filter(|c| c.len() == hex_len)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
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
        let mut sorted: Vec<&UrlEntry> = self.urls.iter().collect();
        sorted.sort_by_key(|a| a.priority);
        sorted.into_iter().next()
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
            HashAlgorithm::Sha256,
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

/// Resolve a potentially relative URL against a base URI.
///
/// Mirrors C++ `MetalinkParserController.cc:175-192` which resolves
/// relative URLs found in Metalink documents against the base URI
/// of the Metalink file itself.
pub fn resolve_url(base_uri: Option<&str>, url: &str) -> String {
    // If URL is already absolute, return as-is
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("ftp://") {
        return url.to_string();
    }

    let Some(base) = base_uri else {
        return url.to_string();
    };

    // Try to resolve relative URL against base
    if let Ok(base_url) = url::Url::parse(base)
        && let Ok(resolved) = base_url.join(url)
    {
        return resolved.to_string();
    }

    // Fallback: return original URL
    url.to_string()
}

/// Group MetalinkFile entries by their first metaurl's URL.
///
/// Mirrors C++ `metalink::groupEntryByMetaurlName()` from `metalink_helper.cc`.
///
/// The grouping logic:
/// - Entries with **no metaurls** form their own group with an empty metaurl key.
/// - Entries whose first metaurl has an **empty name** or whose **size is unknown**
///   always start a new group (they cannot be merged into an existing group).
/// - Otherwise, the entry is merged into an existing group if its first metaurl URL
///   matches the group's key AND the group's first entry has a non-empty name.
/// - If no matching group is found, a new group is created.
///
/// Returns a vector of `(metaurl_key, Vec<index>)` where `index` refers to the
/// position within the input `files` slice.
pub fn group_entry_by_metaurl_name(files: &[MetalinkFile]) -> Vec<(String, Vec<usize>)> {
    let mut result: Vec<(String, Vec<usize>)> = Vec::new();

    for (idx, file) in files.iter().enumerate() {
        if file.meta_urls.is_empty() {
            // No metaurls → standalone group with empty key
            result.push((String::new(), vec![idx]));
        } else {
            let meta_url = &file.meta_urls[0];
            // C++ condition: if name is empty or size is unknown, skip merge search
            let can_merge =
                meta_url.name.as_ref().is_some_and(|n| !n.is_empty()) && file.size_known;

            let mut found = false;
            if can_merge {
                for group in &mut result {
                    let group_first_has_name = files[group.1[0]]
                        .meta_urls
                        .first()
                        .and_then(|m| m.name.as_deref())
                        .is_some_and(|n| !n.is_empty());
                    if group.0 == meta_url.url && group_first_has_name {
                        group.1.push(idx);
                        found = true;
                        break;
                    }
                }
            }

            if !found {
                result.push((meta_url.url.clone(), vec![idx]));
            }
        }
    }

    result
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

fn bts(b: &[u8]) -> String {
    std::str::from_utf8(b).unwrap_or("").trim().to_string()
}

fn collect_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .flatten()
        .map(|a| (bts(a.key.as_ref()), bts(&a.value)))
        .collect()
}

fn find_attr(attrs: &[(String, String)], key: &str) -> String {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Detect directory traversal in a file name.
///
/// Mirrors C++ `util::detectDirTraversal()` from `util.cc:2259-2274`.
/// Returns `true` if the name contains path traversal sequences or
/// control characters that could be used for security exploits.
fn detect_dir_traversal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Control characters (0x00-0x1F, 0x7F)
    if s.chars().any(|c| c.is_control()) {
        return true;
    }
    // Exact matches
    if s == "." || s == ".." {
        return true;
    }
    // Starts with
    if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") {
        return true;
    }
    // Contains
    if s.contains("/../") || s.contains("/./") {
        return true;
    }
    // Ends with
    if s.ends_with('/') || s.ends_with("/.") || s.ends_with("/..") {
        return true;
    }
    false
}

/// Sanitize a Metalink file name by rejecting directory traversal attempts.
///
/// Returns `None` if the name is rejected (traversal detected or empty).
/// Returns `Some(sanitized)` with the original name if it passes validation.
fn sanitize_file_name(name: &str) -> Option<String> {
    if name.is_empty() || detect_dir_traversal(name) {
        return None;
    }
    Some(name.to_string())
}

impl MetalinkDocument {
    pub fn parse(data: &[u8], base_uri: Option<&str>) -> Result<Self, String> {
        use quick_xml::{Reader, events::Event};

        let mut reader = Reader::from_reader(data);

        // Pre-compute the resolved base URI once so that resolve_url()
        // can borrow it for every <url>/<metaurl> element without
        // repeatedly re-parsing the same string.
        let base_uri_owned: Option<String> = base_uri.map(|s| s.to_string());
        let base_uri_ref: Option<&str> = base_uri_owned.as_deref();

        let mut doc = Self {
            version: MetalinkVersion::V4,
            files: Vec::new(),
            generator: None,
            origin: None,
            published: None,
            base_uri: None, // Assigned after parsing loop to avoid cloning.
        };

        let mut current_file: Option<MetalinkFile> = None;
        let mut text_buf = String::new();
        let mut pending_attrs: Vec<(String, String)> = Vec::new();
        let mut saw_files_wrapper = false;
        // `<pieces>` state: `Some((length, algo))` while inside the element.
        // `pieces_sub_element` turns true when v4 `<hash>` children are seen
        // (each child is one digest); otherwise the character data is treated
        // as v3-style concatenated digests and chunked on element end.
        let mut pending_pieces: Option<(u32, HashAlgorithm)> = None;
        let mut pieces_sub_element = false;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let tag = bts(e.local_name().as_ref());
                    match tag.as_str() {
                        "metalink" => {
                            let attrs = collect_attrs(&e);
                            for (key, val) in &attrs {
                                if key == "xmlns" {
                                    // V4 namespace: urn:ietf:params:xml:ns:metalink
                                    // V3 namespace: http://www.metalinker.org/
                                    if val == "urn:ietf:params:xml:ns:metalink" {
                                        doc.version = MetalinkVersion::V4;
                                    } else if val == "http://www.metalinker.org/" {
                                        doc.version = MetalinkVersion::V3;
                                    }
                                }
                            }
                        }
                        "files" => {
                            saw_files_wrapper = true;
                        }
                        "file" => {
                            let name = find_attr(&collect_attrs(&e), "name");
                            let file_name = if name.is_empty() {
                                format!("unknown_{}", doc.files.len())
                            } else {
                                match sanitize_file_name(&name) {
                                    Some(safe_name) => safe_name,
                                    None => {
                                        tracing::warn!(
                                            name = %name,
                                            "Rejecting Metalink file name with directory traversal"
                                        );
                                        format!("unknown_{}", doc.files.len())
                                    }
                                }
                            };
                            current_file = Some(MetalinkFile::new(&file_name));
                        }
                        "pieces" => {
                            // `<pieces length="N" type="sha-256">` — V4 uses
                            // `<hash>` children (one digest each); V3-style
                            // docs may inline the digests as element text.
                            let attrs = collect_attrs(&e);
                            let len_s = find_attr(&attrs, "length");
                            let type_s = find_attr(&attrs, "type");
                            let length: u32 = len_s.parse().unwrap_or(0);
                            let algo =
                                HashAlgorithm::parse(&type_s).unwrap_or(HashAlgorithm::Sha256);
                            pending_pieces = Some((length, algo));
                            pieces_sub_element = false;
                            if let Some(ref mut f) = current_file {
                                f.pieces = Some(PieceInfo {
                                    length,
                                    type_: algo,
                                    hashes: Vec::new(),
                                });
                            }
                            text_buf.clear();
                            pending_attrs = attrs;
                        }
                        "hash" => {
                            if pending_pieces.is_some() {
                                // V4: `<pieces>` children — each `<hash>` is
                                // one complete digest of one piece.
                                pieces_sub_element = true;
                            }
                            text_buf.clear();
                            pending_attrs = collect_attrs(&e);
                        }
                        _ => {
                            text_buf.clear();
                            pending_attrs = collect_attrs(&e);
                        }
                    }
                }
                Ok(Event::Text(e)) => {
                    text_buf.push_str(bts(&e).trim());
                }
                Ok(Event::End(e)) => {
                    let tag = bts(e.local_name().as_ref());
                    match tag.as_str() {
                        "file" => {
                            if let Some(file) = current_file.take() {
                                doc.files.push(file);
                            }
                        }
                        "size" => {
                            if let Some(ref mut f) = current_file
                                && let Ok(size) = text_buf.trim().parse::<u64>()
                            {
                                f.size = Some(size);
                                f.size_known = true;
                            }
                        }
                        "identity" => {
                            if let Some(ref mut f) = current_file {
                                f.identity = Some(text_buf.clone());
                            }
                        }
                        "version" => {
                            if let Some(ref mut f) = current_file {
                                f.version = Some(text_buf.clone());
                            }
                        }
                        "language" => {
                            if let Some(ref mut f) = current_file {
                                f.languages.push(text_buf.clone());
                            }
                        }
                        "os" => {
                            if let Some(ref mut f) = current_file {
                                f.oses.push(text_buf.clone());
                            }
                        }
                        "hash" => {
                            if pending_pieces.is_some() {
                                // V4 `<pieces>` child: one digest per piece.
                                if let Some(ref mut f) = current_file
                                    && let Some(pi) = f.pieces.as_mut()
                                    && !text_buf.is_empty()
                                {
                                    pi.hashes.push(text_buf.clone());
                                }
                            } else if let Some(ref mut f) = current_file
                                && let Some(algo) =
                                    HashAlgorithm::parse(&find_attr(&pending_attrs, "type"))
                            {
                                f.hashes.push(HashEntry::new(algo, &text_buf));
                            }
                        }
                        "resources" => {
                            // V3 <resources maxconnections="N"> wrapper
                            if let Some(ref mut f) = current_file {
                                let mc = find_attr(&pending_attrs, "maxconnections");
                                if let Ok(n) = mc.parse::<i32>()
                                    && n > 0
                                {
                                    f.max_connections = Some(n);
                                }
                            }
                        }
                        "url" => {
                            if let Some(ref mut f) = current_file {
                                let resolved = resolve_url(base_uri_ref, &text_buf);
                                let mut entry = UrlEntry::new(&resolved);
                                for (key, val) in &pending_attrs {
                                    match key.as_str() {
                                        "priority" => {
                                            if let Ok(p) = val.parse::<i32>() {
                                                entry.priority = p;
                                            }
                                        }
                                        "location" => {
                                            entry.location = Some(val.clone());
                                        }
                                        "max-connections" => {
                                            if let Ok(n) = val.parse::<u32>() {
                                                entry.max_connections = Some(n);
                                            }
                                        }
                                        "preference" => {
                                            if let Ok(p) = val.parse::<i32>() {
                                                // V3 preference: highest value (100) = best.
                                                // V4 priority: lowest value (1) = best.
                                                // Conversion: priority = 101 - preference
                                                // This mirrors C++ MetalinkParserStateV3Impl.cc:355.
                                                if (0..=100).contains(&p) {
                                                    entry.preference = Some(p);
                                                    entry.priority = 101 - p;
                                                } else {
                                                    entry.preference = Some(p);
                                                }
                                            }
                                        }
                                        "type" => {
                                            // V3 <url type="http|https|ftp|bittorrent">
                                            // Maps to MetalinkResource::TYPE.
                                            entry.resource_type =
                                                ResourceType::from_url_type_str(val);
                                        }
                                        _ => {}
                                    }
                                }
                                f.urls.push(entry);
                            }
                        }
                        "metaurl" => {
                            if let Some(ref mut f) = current_file {
                                let resolved = resolve_url(base_uri_ref, &text_buf);
                                let type_attr = find_attr(&pending_attrs, "mediatype");
                                let mut entry =
                                    MetaUrlEntry::new(&resolved, MediaType::parse(&type_attr));
                                for (key, val) in &pending_attrs {
                                    match key.as_str() {
                                        "priority" => {
                                            if let Ok(p) = val.parse::<i32>() {
                                                entry.priority = p;
                                            }
                                        }
                                        "name"
                                            // Reject directory traversal in metaurl@name
                                            // (mirrors C++ MetalinkParserStateV4Impl.cc:108)
                                            if !detect_dir_traversal(val) => {
                                                entry.name = Some(val.clone());
                                            }
                                        _ => {}
                                    }
                                }
                                f.meta_urls.push(entry);
                            }
                        }
                        "pieces" => {
                            if !pieces_sub_element && let Some((length, algo)) = pending_pieces {
                                // V3-style: concatenated digests in the element
                                // text, chunked by the algorithm's hex length.
                                let hashes = split_piece_hashes(&text_buf, algo.hash_len());
                                if let Some(ref mut f) = current_file {
                                    f.pieces = Some(PieceInfo {
                                        length,
                                        type_: algo,
                                        hashes,
                                    });
                                }
                            }
                            pending_pieces = None;
                        }
                        "generator" => {
                            doc.generator = Some(text_buf.clone());
                        }
                        "origin" => {
                            doc.origin = Some(text_buf.clone());
                        }
                        "published" => {
                            doc.published = Some(text_buf.clone());
                        }
                        _ => {}
                    }
                    text_buf.clear();
                    pending_attrs.clear();
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(format!("XML parse error: {}", e)),
                _ => {}
            }
        }

        // Fallback heuristic when no explicit xmlns was matched:
        // V3 documents use a <files> wrapper around <file> elements,
        // while V4 documents have <file> directly under <metalink>.
        // If the namespace detection didn't fire (e.g. no xmlns attribute),
        // use the presence of the <files> wrapper as a hint.
        if doc.version == MetalinkVersion::V4 && saw_files_wrapper {
            // Namespace default was V4 but we saw <files> → likely V3
            doc.version = MetalinkVersion::V3;
        }

        if doc.files.is_empty() {
            return Err("Metalink document contains no files".to_string());
        }

        info!(
            "Metalink parsed: version={}, files={}",
            doc.version.as_str(),
            doc.files.len()
        );
        doc.base_uri = base_uri_owned;
        Ok(doc)
    }

    pub fn single_file(&self) -> Option<&MetalinkFile> {
        if self.files.len() == 1 {
            Some(&self.files[0])
        } else {
            None
        }
    }

    pub fn all_urls(&self) -> Vec<&str> {
        self.files
            .iter()
            .flat_map(|f| f.urls.iter().map(|u| u.url.as_str()))
            .collect()
    }

    pub fn total_size(&self) -> Option<u64> {
        let mut total: u64 = 0;
        for f in &self.files {
            if let Some(size) = f.size {
                total += size;
            }
        }
        if total > 0 || self.files.is_empty() {
            Some(total)
        } else {
            None
        }
    }

    /// Query (filter) file entries matching the given version/language/os criteria.
    ///
    /// Mirrors C++ `Metalinker::queryEntry()`. Returns indices of matching files.
    /// Empty filter strings match everything.
    pub fn query_entries(&self, version: &str, language: &str, os: &str) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                if !version.is_empty() && f.version.as_deref() != Some(version) {
                    return false;
                }
                if !language.is_empty() && !f.contains_language(language) {
                    return false;
                }
                if !os.is_empty() && !f.contains_os(os) {
                    return false;
                }
                true
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Filter file entries by a select-file segment list (1-based indices).
    ///
    /// Mirrors C++ `Metalink2RequestGroup::createRequestGroup()` which
    /// applies `PREF_SELECT_FILE` to keep only the selected files.
    /// The `segments` parameter is a sorted list of 1-based file indices.
    /// Returns a new `MetalinkDocument` containing only the selected files.
    pub fn select_files(&self, segments: &[usize]) -> Self {
        if segments.is_empty() {
            return self.clone();
        }

        let selected: Vec<MetalinkFile> = segments
            .iter()
            .filter_map(|&seg| {
                // Segments are 1-based
                if seg > 0 && seg <= self.files.len() {
                    Some(self.files[seg - 1].clone())
                } else {
                    None
                }
            })
            .collect();

        let mut doc = Self {
            version: self.version,
            files: selected,
            generator: self.generator.clone(),
            origin: self.origin.clone(),
            published: self.published.clone(),
            base_uri: self.base_uri.clone(),
        };
        if doc.files.is_empty() {
            doc.files = self.files.clone();
        }
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_v3_metalink() -> Vec<u8> {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
    <file name="test.iso">
      <size>1048576</size>
      <identity>abc123def456</identity>
      <verification>
        <hash type="sha-256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
        <hash type="sha-1">da39a3ee5e6b4b0d3255bfef95601890afd80709</hash>
      </verification>
      <resources maxconnections="4">
        <url type="http" location="cn" preference="90">http://mirror1.cn/test.iso</url>
        <url type="http" location="us" preference="80">http://mirror2.us/test.iso</url>
      </resources>
    </file>
  </files>
</metalink>"#
            .as_bytes()
            .to_vec()
    }

    fn make_v4_metalink() -> Vec<u8> {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <generator>aria2/1.37.0-Rust</generator>
  <origin>Dynamic</origin>
  <published>2024-01-01T00:00:00Z</published>
  <file name="example.bin">
    <size>2048576</size>
    <identity>fedcba654321</identity>
    <hash type="sha-256">cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2baff41</hash>
    <url priority="1">http://primary.example.com/example.bin</url>
    <url priority="50">http://backup.example.com/example.bin</url>
    <pieces length="262144" type="sha-256">hash1hash2</pieces>
  </file>
</metalink>"#.as_bytes().to_vec()
    }

    #[test]
    fn test_parse_v3_metalink() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        assert_eq!(doc.version, MetalinkVersion::V3);
        assert_eq!(doc.files.len(), 1);
        assert_eq!(doc.files[0].name, "test.iso");
        assert_eq!(doc.files[0].size, Some(1048576));
        // V3 uses <verification><hash> and <resources><url>
        assert_eq!(doc.files[0].urls.len(), 2);
        assert_eq!(doc.files[0].hashes.len(), 2);
        // V3 meta_urls: no <metaurl> in this V3 fixture
        assert_eq!(doc.files[0].meta_urls.len(), 0);
        // V3 preference=90 → priority = 101 - 90 = 11
        // V3 preference=80 → priority = 101 - 80 = 21
        assert_eq!(doc.files[0].urls[0].preference, Some(90));
        assert_eq!(doc.files[0].urls[0].priority, 11);
        assert_eq!(doc.files[0].urls[1].preference, Some(80));
        assert_eq!(doc.files[0].urls[1].priority, 21);
    }

    #[test]
    fn test_parse_v4_metalink() {
        let data = make_v4_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        assert_eq!(doc.version, MetalinkVersion::V4);
        assert_eq!(doc.generator.as_deref(), Some("aria2/1.37.0-Rust"));
        assert_eq!(doc.origin.as_deref(), Some("Dynamic"));
        assert_eq!(doc.published.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(doc.files[0].name, "example.bin");
        assert_eq!(doc.files[0].urls[0].priority, 1);
        assert_eq!(doc.files[0].urls[1].priority, 50);
    }

    #[test]
    fn test_url_sorting() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        let urls = doc.files[0].get_sorted_urls();
        // V3 preference=90 → priority=11, preference=80 → priority=21
        // Lower priority value = tried first (V4 semantics)
        assert_eq!(urls[0].priority, 11);
        assert_eq!(urls[1].priority, 21);
    }

    #[test]
    fn test_preferred_url() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        let preferred = doc.files[0].get_preferred_url();
        assert!(preferred.is_some());
        // V3 preference=90 → priority=11 (best)
        assert_eq!(preferred.unwrap().priority, 11);
    }

    #[test]
    fn test_hash_algorithm_parsing() {
        assert_eq!(HashAlgorithm::parse("md5"), Some(HashAlgorithm::Md5));
        assert_eq!(HashAlgorithm::parse("SHA-256"), Some(HashAlgorithm::Sha256));
        assert_eq!(HashAlgorithm::parse("sha512"), Some(HashAlgorithm::Sha512));
        assert_eq!(HashAlgorithm::parse("unknown"), None);
        assert_eq!(HashAlgorithm::Md5.hash_len(), 32);
        assert_eq!(HashAlgorithm::Sha256.hash_len(), 64);
    }

    #[test]
    fn test_mediatype_detection() {
        assert!(MediaType::parse("torrent").is_torrent());
        assert!(MediaType::parse("application/x-bittorrent").is_torrent());
        assert!(!MediaType::parse("xml").is_torrent());
    }

    #[test]
    fn test_empty_metalink_fails() {
        let bad = b"<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"></metalink>".to_vec();
        assert!(MetalinkDocument::parse(&bad, None).is_err());
    }

    #[test]
    fn test_single_file_accessor() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        assert!(doc.single_file().is_some());
        assert_eq!(doc.single_file().unwrap().name, "test.iso");
    }

    #[test]
    fn test_pieces_info() {
        let data = make_v4_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        let pieces = &doc.files[0].pieces;
        assert!(pieces.is_some());
        let p = pieces.as_ref().unwrap();
        assert_eq!(p.length, 262144);
        assert_eq!(p.type_, HashAlgorithm::Sha256);
        // "hash1hash2" is 10 chars < 64: no complete sha-256 digest, so the
        // text-mode chunker must drop it entirely (count 0, not garbage).
        assert_eq!(p.piece_count(), 0);
    }

    #[test]
    fn test_pieces_concatenated_text_is_chunked() {
        // Two real 64-char sha-256 digests concatenated with no whitespace.
        let h1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let h2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="f.bin">
    <size>524288</size>
    <pieces length="262144" type="sha-256">{h1}{h2}</pieces>
  </file>
</metalink>"#
        );
        let doc = MetalinkDocument::parse(xml.as_bytes(), None).unwrap();
        let p = doc.files[0].pieces.as_ref().unwrap();
        assert_eq!(
            p.piece_count(),
            2,
            "contiguous digests must be chunked by hex length"
        );
        assert_eq!(p.hashes[0], h1);
        assert_eq!(p.hashes[1], h2);
        assert_eq!(p.num_pieces(524288), 2);
    }

    #[test]
    fn test_pieces_v4_hash_children() {
        // V4 spec: `<pieces>` contains one `<hash>` element per piece.
        let h1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let h2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="f.bin">
    <size>524288</size>
    <pieces length="262144" type="sha-256">
      <hash>{h1}</hash>
      <hash>{h2}</hash>
    </pieces>
  </file>
</metalink>"#
        );
        let doc = MetalinkDocument::parse(xml.as_bytes(), None).unwrap();
        let p = doc.files[0].pieces.as_ref().unwrap();
        assert_eq!(
            p.piece_count(),
            2,
            "v4 <hash> children must be collected per piece"
        );
        assert_eq!(p.hashes[0], h1);
        assert_eq!(p.hashes[1], h2);
        assert_eq!(p.length, 262144);
        assert_eq!(p.type_, HashAlgorithm::Sha256);
        // Verification hashes (whole-file) must NOT be polluted by pieces.
        assert!(doc.files[0].hashes.is_empty());
    }

    #[test]
    fn test_all_urls_collector() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data, None).unwrap();
        let urls = doc.all_urls();
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn test_dir_traversal_detection() {
        assert!(detect_dir_traversal(".."));
        assert!(detect_dir_traversal("."));
        assert!(detect_dir_traversal("../etc/passwd"));
        assert!(detect_dir_traversal("./secret"));
        assert!(detect_dir_traversal("/etc/passwd"));
        assert!(detect_dir_traversal("foo/../bar"));
        assert!(detect_dir_traversal("foo/./bar"));
        assert!(detect_dir_traversal("foo/"));
        assert!(detect_dir_traversal("foo/."));
        assert!(detect_dir_traversal("foo/.."));
        assert!(!detect_dir_traversal("normal.txt"));
        assert!(!detect_dir_traversal("path/to/file.iso"));
        assert!(!detect_dir_traversal(""));
    }

    #[test]
    fn test_dir_traversal_in_metalink_filename() {
        let bad = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="../../../etc/passwd">
    <size>100</size>
    <url priority="1">http://example.com/file</url>
  </file>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&bad, None).unwrap();
        // Directory traversal name should be rejected and replaced
        assert_ne!(doc.files[0].name, "../../../etc/passwd");
        assert!(doc.files[0].name.starts_with("unknown_"));
    }

    #[test]
    fn test_namespace_detection_v3() {
        let v3_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
    <file name="test.iso">
      <size>100</size>
      <url type="http" preference="50">http://example.com/test.iso</url>
    </file>
  </files>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&v3_xml, None).unwrap();
        assert_eq!(doc.version, MetalinkVersion::V3);
    }

    #[test]
    fn test_namespace_detection_v4() {
        let v4_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="test.bin">
    <size>100</size>
    <url priority="1">http://example.com/test.bin</url>
  </file>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&v4_xml, None).unwrap();
        assert_eq!(doc.version, MetalinkVersion::V4);
    }

    // ========================================================================
    // ResourceType + V3 type attribute parsing tests
    // ========================================================================

    #[test]
    fn test_resource_type_from_url_type_str() {
        assert_eq!(ResourceType::from_url_type_str("http"), ResourceType::Http);
        assert_eq!(
            ResourceType::from_url_type_str("HTTPS"),
            ResourceType::Https
        );
        assert_eq!(ResourceType::from_url_type_str("ftp"), ResourceType::Ftp);
        assert_eq!(
            ResourceType::from_url_type_str("bittorrent"),
            ResourceType::BitTorrent
        );
        // Unknown type strings map to NotSupported per C++ MetalinkParserController::setTypeOfResource()
        assert_eq!(
            ResourceType::from_url_type_str("unknown"),
            ResourceType::NotSupported
        );
    }

    #[test]
    fn test_resource_type_as_str() {
        assert_eq!(ResourceType::Ftp.as_str(), "ftp");
        assert_eq!(ResourceType::Http.as_str(), "http");
        assert_eq!(ResourceType::Https.as_str(), "https");
        assert_eq!(ResourceType::BitTorrent.as_str(), "bittorrent");
        assert_eq!(ResourceType::NotSupported.as_str(), "not_supported");
        assert_eq!(ResourceType::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_v3_type_attribute_parsed_into_resource_type() {
        let v3_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="http://www.metalinker.org/">
  <files>
    <file name="test.iso">
      <size>1048576</size>
      <resources>
        <url type="http" location="cn" preference="90">http://mirror1.cn/test.iso</url>
        <url type="https" location="us" preference="80">https://mirror2.us/test.iso</url>
        <url type="ftp" location="jp" preference="70">ftp://mirror3.jp/test.iso</url>
        <url type="bittorrent" preference="50">magnet:?xt=urn:btih:abc</url>
        <url type="unknown" preference="40">http://mirror5.unknown/test.iso</url>
      </resources>
    </file>
  </files>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&v3_xml, None).unwrap();
        let urls = &doc.files[0].urls;
        assert_eq!(urls.len(), 5);
        // V3 type="http" should override URL-scheme auto-detection
        assert_eq!(urls[0].resource_type, ResourceType::Http);
        assert_eq!(urls[1].resource_type, ResourceType::Https);
        assert_eq!(urls[2].resource_type, ResourceType::Ftp);
        assert_eq!(urls[3].resource_type, ResourceType::BitTorrent);
        // Unknown type strings → NotSupported per C++ behavior
        assert_eq!(urls[4].resource_type, ResourceType::NotSupported);
    }

    #[test]
    fn test_v4_url_auto_detects_resource_type() {
        // V4 has no type attribute; resource_type is auto-detected from URL scheme
        let v4_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="test.bin">
    <size>100</size>
    <url priority="1">http://example.com/test.bin</url>
    <url priority="2">https://example.com/test.bin</url>
    <url priority="3">ftp://example.com/test.bin</url>
  </file>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&v4_xml, None).unwrap();
        assert_eq!(doc.files[0].urls[0].resource_type, ResourceType::Http);
        assert_eq!(doc.files[0].urls[1].resource_type, ResourceType::Https);
        assert_eq!(doc.files[0].urls[2].resource_type, ResourceType::Ftp);
    }

    // ========================================================================
    // MetalinkFile method tests
    // ========================================================================

    #[test]
    fn test_drop_unsupported_resources() {
        let mut file = MetalinkFile::new("test.bin");
        file.urls
            .push(UrlEntry::new("http://a.com/f").with_resource_type(ResourceType::Http));
        file.urls
            .push(UrlEntry::new("https://b.com/f").with_resource_type(ResourceType::Https));
        file.urls
            .push(UrlEntry::new("ftp://c.com/f").with_resource_type(ResourceType::Ftp));
        file.urls.push(
            UrlEntry::new("magnet:?xt=urn:btih:abc").with_resource_type(ResourceType::BitTorrent),
        );
        file.urls
            .push(UrlEntry::new("http://d.com/f").with_resource_type(ResourceType::NotSupported));
        file.urls
            .push(UrlEntry::new("http://e.com/f").with_resource_type(ResourceType::Unknown));
        file.drop_unsupported_resources();
        // Both NotSupported and Unknown are removed, matching C++ default case
        assert_eq!(file.urls.len(), 4);
        assert!(file.urls.iter().all(|u| u.is_supported()));
    }

    #[test]
    fn test_set_location_priority() {
        let mut file = MetalinkFile::new("test.bin");
        file.urls.push(
            UrlEntry::new("http://a.com/f")
                .with_location("cn")
                .with_priority(10),
        );
        file.urls.push(
            UrlEntry::new("http://b.com/f")
                .with_location("us")
                .with_priority(10),
        );
        file.urls.push(
            UrlEntry::new("http://c.com/f")
                .with_location("jp")
                .with_priority(10),
        );
        file.urls.push(UrlEntry::new("http://d.com/f")); // no location

        // Boost cn and jp locations by -999999 (mirrors C++ usage)
        file.set_location_priority(&["cn", "jp"], -999999);

        assert_eq!(file.urls[0].priority, 10 - 999999);
        assert_eq!(file.urls[1].priority, 10); // us: unchanged
        assert_eq!(file.urls[2].priority, 10 - 999999);
        assert_eq!(file.urls[3].priority, 999999); // no location: unchanged
    }

    #[test]
    fn test_set_protocol_priority() {
        let mut file = MetalinkFile::new("test.bin");
        file.urls.push(
            UrlEntry::new("http://a.com/f")
                .with_resource_type(ResourceType::Http)
                .with_priority(10),
        );
        file.urls.push(
            UrlEntry::new("https://b.com/f")
                .with_resource_type(ResourceType::Https)
                .with_priority(10),
        );
        file.urls.push(
            UrlEntry::new("ftp://c.com/f")
                .with_resource_type(ResourceType::Ftp)
                .with_priority(10),
        );

        // Boost https by -999999 (mirrors C++ usage with preferred protocol)
        file.set_protocol_priority("https", -999999);

        assert_eq!(file.urls[0].priority, 10);
        assert_eq!(file.urls[1].priority, 10 - 999999);
        assert_eq!(file.urls[2].priority, 10);
    }

    #[test]
    fn test_reorder_resources_by_priority() {
        let mut file = MetalinkFile::new("test.bin");
        file.urls
            .push(UrlEntry::new("http://a.com/f").with_priority(30));
        file.urls
            .push(UrlEntry::new("http://b.com/f").with_priority(10));
        file.urls
            .push(UrlEntry::new("http://c.com/f").with_priority(20));

        file.reorder_resources_by_priority();

        // After shuffle+sort, must be in ascending priority order
        assert_eq!(file.urls[0].priority, 10);
        assert_eq!(file.urls[1].priority, 20);
        assert_eq!(file.urls[2].priority, 30);
    }

    #[test]
    fn test_reorder_metaurls_by_priority() {
        let mut file = MetalinkFile::new("test.bin");
        file.meta_urls
            .push(MetaUrlEntry::new("http://a.com/torrent", MediaType::Torrent).with_priority(30));
        file.meta_urls
            .push(MetaUrlEntry::new("http://b.com/torrent", MediaType::Torrent).with_priority(10));
        file.meta_urls
            .push(MetaUrlEntry::new("http://c.com/torrent", MediaType::Torrent).with_priority(20));

        file.reorder_metaurls_by_priority();

        assert_eq!(file.meta_urls[0].priority, 10);
        assert_eq!(file.meta_urls[1].priority, 20);
        assert_eq!(file.meta_urls[2].priority, 30);
    }

    // ========================================================================
    // group_entry_by_metaurl_name tests
    // ========================================================================

    #[test]
    fn test_group_entry_no_metaurls() {
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size = Some(100);
        f1.size_known = true;
        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // No metaurls → each gets its own group with empty key
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "");
        assert_eq!(groups[0].1, vec![0]);
        assert_eq!(groups[1].0, "");
        assert_eq!(groups[1].1, vec![1]);
    }

    #[test]
    fn test_group_entry_same_metaurl_merges() {
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size = Some(100);
        f1.size_known = true;
        f1.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("a.bin")
            .with_priority(1),
        );

        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;
        f2.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("b.bin")
            .with_priority(1),
        );

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // Same metaurl URL, both have names, both size_known → merged
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "http://torrent.example.com/meta.torrent");
        assert_eq!(groups[0].1, vec![0, 1]);
    }

    #[test]
    fn test_group_entry_empty_name_no_merge() {
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size = Some(100);
        f1.size_known = true;
        f1.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_priority(1),
            // name is None
        );

        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;
        f2.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("b.bin")
            .with_priority(1),
        );

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // f1 has no name → cannot merge; f2 gets its own group
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_group_entry_size_unknown_no_merge() {
        // When an entry has size_unknown, it cannot initiate a merge search,
        // but other entries with size_known can still merge into it.
        // This mirrors C++ where !entry->sizeKnown just skips the search loop
        // for that entry, but the group's first entry only needs a non-empty name
        // for others to merge into.
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size_known = false; // size unknown
        f1.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("a.bin")
            .with_priority(1),
        );

        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;
        f2.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("b.bin")
            .with_priority(1),
        );

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // f1 size unknown → cannot search for merge → creates new group
        // f2 size known → searches, finds f1's group (same URL, f1 has name) → merges
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, vec![0, 1]);
    }

    #[test]
    fn test_group_entry_size_unknown_and_no_name() {
        // When the first entry in a group has no name, subsequent entries
        // cannot merge into it (group_first_has_name check fails).
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size_known = false;
        f1.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_priority(1), // no name
        );

        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;
        f2.meta_urls.push(
            MetaUrlEntry::new(
                "http://torrent.example.com/meta.torrent",
                MediaType::Torrent,
            )
            .with_name("b.bin")
            .with_priority(1),
        );

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // f1 has no name → f2 cannot merge into it (group_first_has_name is false)
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_group_entry_different_metaurl_no_merge() {
        let mut f1 = MetalinkFile::new("a.bin");
        f1.size = Some(100);
        f1.size_known = true;
        f1.meta_urls.push(
            MetaUrlEntry::new("http://a.com/meta.torrent", MediaType::Torrent)
                .with_name("a.bin")
                .with_priority(1),
        );

        let mut f2 = MetalinkFile::new("b.bin");
        f2.size = Some(200);
        f2.size_known = true;
        f2.meta_urls.push(
            MetaUrlEntry::new("http://b.com/meta.torrent", MediaType::Torrent)
                .with_name("b.bin")
                .with_priority(1),
        );

        let groups = group_entry_by_metaurl_name(&[f1, f2]);
        // Different metaurl URLs → separate groups
        assert_eq!(groups.len(), 2);
    }
}
