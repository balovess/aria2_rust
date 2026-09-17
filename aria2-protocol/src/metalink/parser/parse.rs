use super::super::resource::ResourceType;
use super::model::{
    HashAlgorithm, HashEntry, MediaType, MetaUrlEntry, MetalinkDocument, MetalinkFile,
    MetalinkVersion, PieceInfo, UrlEntry,
};
use tracing::info;

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
pub(super) fn detect_dir_traversal(s: &str) -> bool {
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
}
