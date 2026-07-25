//! Metalink Entry — represents a single file within a Metalink document.
//!
//! A Metalink document contains one or more file entries, each with
//! its own set of URLs (mirrors), hashes, and metadata.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `MetalinkEntry` | `MetalinkEntry` |
//! | `FileEntry` | `FileEntry` (from `File.h`) |
//!
//! Currently, the main file-entry types live in [`parser`](super::parser)
//! as [`MetalinkFile`], [`UrlEntry`], and [`HashEntry`]. This module
//! provides the top-level `MetalinkEntry` wrapper that maps to the
//! C++ `MetalinkEntry` class used in `MetalinkEntry.h`.

use super::parser::{HashEntry, MetalinkFile, UrlEntry};

/// A single file entry within a Metalink document.
///
/// Wraps the parsed file data from [`MetalinkFile`] and provides
/// the high-level interface that `MetalinkDownloadCommand` uses.
///
/// Mirrors C++ `MetalinkEntry` which aggregates `FileEntry` metadata
/// with Metalink-specific properties (max connections, preference, etc.).
#[derive(Debug, Clone)]
pub struct MetalinkEntry {
    /// The parsed file metadata
    pub file: MetalinkFile,
}

impl MetalinkEntry {
    /// Create a new MetalinkEntry wrapping the given file.
    pub fn new(file: MetalinkFile) -> Self {
        Self { file }
    }

    /// Return the file name.
    pub fn name(&self) -> &str {
        &self.file.name
    }

    /// Return the file size in bytes.
    pub fn size(&self) -> Option<u64> {
        self.file.size
    }

    /// Return true if the file size was explicitly specified.
    /// Mirrors C++ `MetalinkEntry::sizeKnown`.
    pub fn size_known(&self) -> bool {
        self.file.size_known
    }

    /// Return the list of HTTP/FTP mirror URLs for this file.
    pub fn urls(&self) -> &[UrlEntry] {
        &self.file.urls
    }

    /// Return the list of hash checksums for verification.
    pub fn hashes(&self) -> &[HashEntry] {
        &self.file.hashes
    }

    /// Return the strongest available hash.
    /// Mirrors C++ `MetalinkParserController` strongest-hash-wins logic.
    pub fn strongest_hash(&self) -> Option<&HashEntry> {
        self.file.strongest_hash()
    }

    /// Return the version string, if specified.
    /// Mirrors C++ `MetalinkEntry::version`.
    pub fn version(&self) -> Option<&str> {
        self.file.version.as_deref()
    }

    /// Return the language codes.
    /// Mirrors C++ `MetalinkEntry::languages`.
    pub fn languages(&self) -> &[String] {
        &self.file.languages
    }

    /// Return the OS codes.
    /// Mirrors C++ `MetalinkEntry::oses`.
    pub fn oses(&self) -> &[String] {
        &self.file.oses
    }

    /// Return the max connections, if specified (V3 only).
    /// Mirrors C++ `MetalinkEntry::maxConnections`.
    pub fn max_connections(&self) -> Option<i32> {
        self.file.max_connections
    }

    /// Check if this entry matches the given version/language/os filter.
    ///
    /// Empty filter strings match everything, matching C++ query logic
    /// in `Metalinker::queryEntry()`.
    pub fn matches_query(&self, version: &str, language: &str, os: &str) -> bool {
        if !version.is_empty() {
            if self.file.version.as_deref() != Some(version) {
                return false;
            }
        }
        if !language.is_empty() && !self.file.contains_language(language) {
            return false;
        }
        if !os.is_empty() && !self.file.contains_os(os) {
            return false;
        }
        true
    }

    /// Convert a slice of `MetalinkEntry` items into a vector of `FileEntry`
    /// objects suitable for creating download request groups.
    ///
    /// Mirrors C++ `MetalinkEntry::toFileEntry()`.
    pub fn to_file_entries(entries: &[MetalinkEntry]) -> Vec<MetalinkFile> {
        entries.iter().map(|e| e.file.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metalink::parser::MetalinkDocument;
    use crate::metalink::parser::HashAlgorithm;

    fn make_test_entry() -> MetalinkEntry {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="test.bin">
    <size>1048576</size>
    <version>1.0</version>
    <language>en</language>
    <language>zh</language>
    <os>Linux</os>
    <hash type="sha-256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
    <hash type="md5">d41d8cd98f00b204e9800998ecf8427e</hash>
    <url priority="1">http://example.com/test.bin</url>
  </file>
</metalink>"#
            .as_bytes()
            .to_vec();
        let doc = MetalinkDocument::parse(&xml, None).unwrap();
        MetalinkEntry::new(doc.files[0].clone())
    }

    #[test]
    fn test_entry_accessors() {
        let entry = make_test_entry();
        assert_eq!(entry.name(), "test.bin");
        assert_eq!(entry.size(), Some(1048576));
        assert!(entry.size_known());
        assert_eq!(entry.version(), Some("1.0"));
        assert_eq!(entry.languages().len(), 2);
        assert_eq!(entry.oses().len(), 1);
    }

    #[test]
    fn test_strongest_hash() {
        let entry = make_test_entry();
        let hash = entry.strongest_hash().unwrap();
        assert_eq!(hash.algo, HashAlgorithm::Sha256);
    }

    #[test]
    fn test_matches_query() {
        let entry = make_test_entry();
        assert!(entry.matches_query("", "", ""));
        assert!(entry.matches_query("1.0", "", ""));
        assert!(entry.matches_query("", "en", ""));
        assert!(entry.matches_query("", "", "Linux"));
        assert!(!entry.matches_query("2.0", "", ""));
        assert!(!entry.matches_query("", "fr", ""));
        assert!(!entry.matches_query("", "", "Windows"));
    }
}
