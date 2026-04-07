use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetalinkVersion {
    V3,
    V4,
}

impl MetalinkVersion {
    pub fn as_str(&self) -> &'static str {
        match self { Self::V3 => "V3", Self::V4 => "V4" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm { Md5, Sha1, Sha256, Sha512 }

impl HashAlgorithm {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "md5" | "md5sum" => Some(Self::Md5),
            "sha-1" | "sha1" | "sha1sum" => Some(Self::Sha1),
            "sha-256" | "sha256" | "sha256sum" => Some(Self::Sha256),
            "sha-512" | "sha512" | "sha512sum" => Some(Self::Sha512),
            _ => None,
        }
    }

    pub fn hash_len(&self) -> usize {
        match self { Self::Md5 => 32, Self::Sha1 => 40, Self::Sha256 => 64, Self::Sha512 => 128 }
    }

    pub fn as_standard_name(&self) -> &'static str {
        match self { Self::Md5 => "md5", Self::Sha1 => "sha-1", Self::Sha256 => "sha-256", Self::Sha512 => "sha-512" }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HashEntry {
    pub algo: HashAlgorithm,
    pub value: String,
}

impl HashEntry {
    pub fn new(algo: HashAlgorithm, value: &str) -> Self {
        Self { algo, value: value.trim().to_lowercase() }
    }

    pub fn is_valid(&self) -> bool { self.value.len() == self.algo.hash_len() }
}

#[derive(Debug, Clone)]
pub struct UrlEntry {
    pub url: String,
    pub priority: i32,
    pub location: Option<String>,
    pub max_connections: Option<u32>,
    pub preference: Option<i32>,
}

impl UrlEntry {
    pub fn new(url: &str) -> Self {
        Self { url: url.trim().to_string(), priority: 0, location: None, max_connections: None, preference: None }
    }

    pub fn with_priority(mut self, p: i32) -> Self { self.priority = p; self }
    pub fn with_location(mut self, loc: &str) -> Self { self.location = Some(loc.to_string()); self }
    pub fn with_max_connections(mut self, n: u32) -> Self { self.max_connections = Some(n); self }
    pub fn with_preference(mut self, p: i32) -> Self { self.preference = Some(p); self }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaType { Torrent, Xml, Other(String) }

impl MediaType {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "application/x-bittorrent" | "torrent" => Self::Torrent,
            "application/xml" | "text/xml" | "xml" => Self::Xml,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn is_torrent(&self) -> bool { matches!(self, Self::Torrent) }
}

#[derive(Debug, Clone)]
pub struct MetaUrlEntry {
    pub url: String,
    pub mediatype: MediaType,
    pub priority: i32,
    pub name: Option<String>,
}

impl MetaUrlEntry {
    pub fn new(url: &str, mediatype: MediaType) -> Self {
        Self { url: url.trim().to_string(), mediatype, priority: 0, name: None }
    }

    pub fn with_priority(mut self, p: i32) -> Self { self.priority = p; self }
    pub fn with_name(mut self, n: &str) -> Self { self.name = Some(n.to_string()); self }
}

#[derive(Debug, Clone)]
pub struct PieceInfo {
    pub length: u32,
    pub type_: HashAlgorithm,
    pub hashes: Vec<String>,
}

impl PieceInfo {
    pub fn num_pieces(&self, file_size: u64) -> usize {
        if self.length == 0 || file_size == 0 { return 0; }
        ((file_size + self.length as u64 - 1) / self.length as u64) as usize
    }

    pub fn piece_count(&self) -> usize { self.hashes.len() / (self.type_.hash_len() / 2) }
}

#[derive(Debug, Clone)]
pub struct MetalinkFile {
    pub name: String,
    pub size: Option<u64>,
    pub identity: Option<String>,
    pub hashes: Vec<HashEntry>,
    pub urls: Vec<UrlEntry>,
    pub meta_urls: Vec<MetaUrlEntry>,
    pub pieces: Option<PieceInfo>,
}

impl MetalinkFile {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), size: None, identity: None, hashes: Vec::new(), urls: Vec::new(), meta_urls: Vec::new(), pieces: None }
    }

    pub fn get_preferred_url(&self) -> Option<&UrlEntry> {
        let mut sorted: Vec<&UrlEntry> = self.urls.iter().collect();
        sorted.sort_by(|a, b| a.priority.cmp(&b.priority));
        sorted.into_iter().next()
    }

    pub fn get_sorted_urls(&self) -> Vec<&UrlEntry> {
        let mut sorted: Vec<&UrlEntry> = self.urls.iter().collect();
        sorted.sort_by(|a, b| a.priority.cmp(&b.priority));
        sorted
    }

    pub fn get_hash(&self, algo: HashAlgorithm) -> Option<&HashEntry> { self.hashes.iter().find(|h| h.algo == algo) }
    pub fn has_torrent_metaurl(&self) -> bool { self.meta_urls.iter().any(|m| m.mediatype.is_torrent()) }
    pub fn total_size(&self) -> Option<u64> { self.size }
}

#[derive(Debug, Clone)]
pub struct MetalinkDocument {
    pub version: MetalinkVersion,
    pub files: Vec<MetalinkFile>,
    pub generator: Option<String>,
    pub origin: Option<String>,
    pub published: Option<String>,
}

fn bts(b: &[u8]) -> String { std::str::from_utf8(b).unwrap_or("").trim().to_string() }

fn collect_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
    e.attributes().flatten()
        .map(|a| (bts(a.key.as_ref()), bts(&a.value)))
        .collect()
}

fn find_attr(attrs: &[(String, String)], key: &str) -> String {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default()
}

impl MetalinkDocument {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        use quick_xml::{Reader, events::Event};

        let mut reader = Reader::from_reader(data);

        let mut doc = Self {
            version: MetalinkVersion::V4,
            files: Vec::new(),
            generator: None,
            origin: None,
            published: None,
        };

        let mut current_file: Option<MetalinkFile> = None;
        let mut text_buf = String::new();
        let mut pending_attrs: Vec<(String, String)> = Vec::new();
        let mut saw_files_wrapper = false;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let tag = bts(e.local_name().as_ref());
                    match tag.as_str() {
                        "metalink" => {
                            let attrs = collect_attrs(&e);
                            for (key, val) in &attrs {
                                if key == "xmlns" && val.contains("metalink") {
                                    if val.contains("ns.ietf") || val.contains("rfc6249") {
                                        doc.version = MetalinkVersion::V3;
                                    } else {
                                        doc.version = MetalinkVersion::V4;
                                    }
                                }
                            }
                        }
                        "files" => {
                            saw_files_wrapper = true;
                        }
                        "file" => {
                            let name = find_attr(&collect_attrs(&e), "name");
                            let file_name = if name.is_empty() { format!("unknown_{}", doc.files.len()) } else { name };
                            current_file = Some(MetalinkFile::new(&file_name));
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
                        "size" => if let Some(ref mut f) = current_file {
                            f.size = text_buf.trim().parse::<u64>().ok();
                        }
                        "identity" => if let Some(ref mut f) = current_file {
                            f.identity = Some(text_buf.clone());
                        }
                        "hash" => if let Some(ref mut f) = current_file {
                            if let Some(algo) = HashAlgorithm::from_str(&find_attr(&pending_attrs, "type")) {
                                f.hashes.push(HashEntry::new(algo, &text_buf));
                            }
                        }
                        "url" => if let Some(ref mut f) = current_file {
                            let mut entry = UrlEntry::new(&text_buf);
                            for (key, val) in &pending_attrs {
                                match key.as_str() {
                                    "priority" => { if let Ok(p) = val.parse::<i32>() { entry.priority = p; } }
                                    "location" => { entry.location = Some(val.clone()); }
                                    "max-connections" => { if let Ok(n) = val.parse::<u32>() { entry.max_connections = Some(n); } }
                                    "preference" => { if let Ok(p) = val.parse::<i32>() { entry.preference = Some(p); } }
                                    _ => {}
                                }
                            }
                            f.urls.push(entry);
                        }
                        "metaurl" => if let Some(ref mut f) = current_file {
                            let type_attr = find_attr(&pending_attrs, "mediatype");
                            let mut entry = MetaUrlEntry::new(&text_buf, MediaType::from_str(&type_attr));
                            for (key, val) in &pending_attrs {
                                match key.as_str() {
                                    "priority" => { if let Ok(p) = val.parse::<i32>() { entry.priority = p; } }
                                    "name" => { entry.name = Some(val.clone()); }
                                    _ => {}
                                }
                            }
                            f.meta_urls.push(entry);
                        }
                        "pieces" => if let Some(ref mut f) = current_file {
                            let len_s = find_attr(&pending_attrs, "length");
                            let type_s = find_attr(&pending_attrs, "type");
                            let length: u32 = len_s.parse().unwrap_or(0);
                            let algo = HashAlgorithm::from_str(&type_s).unwrap_or(HashAlgorithm::Sha256);
                            let hashes: Vec<String> = text_buf.split_whitespace().map(|s| s.to_string()).collect();
                            f.pieces = Some(PieceInfo { length, type_: algo, hashes });
                        }
                        "generator" => { doc.generator = Some(text_buf.clone()); }
                        "origin" => { doc.origin = Some(text_buf.clone()); }
                        "published" => { doc.published = Some(text_buf.clone()); }
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

        if !saw_files_wrapper && doc.version == MetalinkVersion::V3 {
            doc.version = MetalinkVersion::V4;
        }
        if saw_files_wrapper && doc.version == MetalinkVersion::V4 {
            doc.version = MetalinkVersion::V3;
        }

        if doc.files.is_empty() {
            return Err("Metalink document contains no files".to_string());
        }

        info!("Metalink parsed: version={}, files={}", doc.version.as_str(), doc.files.len());
        Ok(doc)
    }

    pub fn single_file(&self) -> Option<&MetalinkFile> {
        if self.files.len() == 1 { Some(&self.files[0]) } else { None }
    }

    pub fn all_urls(&self) -> Vec<&str> {
        self.files.iter().flat_map(|f| f.urls.iter().map(|u| u.url.as_str())).collect()
    }

    pub fn total_size(&self) -> Option<u64> {
        let mut total: u64 = 0;
        for f in &self.files {
            if let Some(size) = f.size { total += size; }
        }
        if total > 0 || self.files.is_empty() { Some(total) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_v3_metalink() -> Vec<u8> {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="test.iso">
      <size>1048576</size>
      <identity>abc123def456</identity>
      <hash type="sha-256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
      <hash type="sha-1">da39a3ee5e6b4b0d3255bfef95601890afd80709</hash>
      <url location="cn" priority="10">http://mirror1.cn/test.iso</url>
      <url location="us" priority="20">http://mirror2.us/test.iso</url>
      <metaurl mediatype="torrent" priority="5">http://example.com/test.torrent</metaurl>
    </file>
  </files>
</metalink>"#.as_bytes().to_vec()
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
        let doc = MetalinkDocument::parse(&data).unwrap();
        assert_eq!(doc.version, MetalinkVersion::V3);
        assert_eq!(doc.files.len(), 1);
        assert_eq!(doc.files[0].name, "test.iso");
        assert_eq!(doc.files[0].size, Some(1048576));
        assert_eq!(doc.files[0].urls.len(), 2);
        assert_eq!(doc.files[0].hashes.len(), 2);
        assert_eq!(doc.files[0].meta_urls.len(), 1);
    }

    #[test]
    fn test_parse_v4_metalink() {
        let data = make_v4_metalink();
        let doc = MetalinkDocument::parse(&data).unwrap();
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
        let doc = MetalinkDocument::parse(&data).unwrap();
        let urls = doc.files[0].get_sorted_urls();
        assert_eq!(urls[0].priority, 10);
        assert_eq!(urls[1].priority, 20);
    }

    #[test]
    fn test_preferred_url() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data).unwrap();
        let preferred = doc.files[0].get_preferred_url();
        assert!(preferred.is_some());
        assert_eq!(preferred.unwrap().priority, 10);
    }

    #[test]
    fn test_hash_algorithm_parsing() {
        assert_eq!(HashAlgorithm::from_str("md5"), Some(HashAlgorithm::Md5));
        assert_eq!(HashAlgorithm::from_str("SHA-256"), Some(HashAlgorithm::Sha256));
        assert_eq!(HashAlgorithm::from_str("sha512"), Some(HashAlgorithm::Sha512));
        assert_eq!(HashAlgorithm::from_str("unknown"), None);
        assert_eq!(HashAlgorithm::Md5.hash_len(), 32);
        assert_eq!(HashAlgorithm::Sha256.hash_len(), 64);
    }

    #[test]
    fn test_mediatype_detection() {
        assert!(MediaType::from_str("torrent").is_torrent());
        assert!(MediaType::from_str("application/x-bittorrent").is_torrent());
        assert!(!MediaType::from_str("xml").is_torrent());
    }

    #[test]
    fn test_empty_metalink_fails() {
        let bad = b"<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"></metalink>".to_vec();
        assert!(MetalinkDocument::parse(&bad).is_err());
    }

    #[test]
    fn test_single_file_accessor() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data).unwrap();
        assert!(doc.single_file().is_some());
        assert_eq!(doc.single_file().unwrap().name, "test.iso");
    }

    #[test]
    fn test_pieces_info() {
        let data = make_v4_metalink();
        let doc = MetalinkDocument::parse(&data).unwrap();
        let pieces = &doc.files[0].pieces;
        assert!(pieces.is_some());
        let p = pieces.as_ref().unwrap();
        assert_eq!(p.length, 262144);
        assert_eq!(p.type_, HashAlgorithm::Sha256);
    }

    #[test]
    fn test_all_urls_collector() {
        let data = make_v3_metalink();
        let doc = MetalinkDocument::parse(&data).unwrap();
        let urls = doc.all_urls();
        assert_eq!(urls.len(), 2);
    }
}
