//! Minimal Torrent Builder for E2E testing
//!
#![allow(dead_code)]
//! Constructs valid .torrent files in bencode format without requiring
//! external torrent parsing libraries.

use sha1::{Digest, Sha1};

/// Bencode encoding helper functions
fn encode_bencode_string(s: &str) -> Vec<u8> {
    format!("{}:{}", s.len(), s).into_bytes()
}

fn encode_bencode_int(i: i64) -> Vec<u8> {
    format!("i{}e", i).into_bytes()
}

fn encode_bencode_bytes(data: &[u8]) -> Vec<u8> {
    let mut result = format!("{}:", data.len()).into_bytes();
    result.extend_from_slice(data);
    result
}

struct TorrentFile {
    length: u64,
    path: Vec<String>,
}

pub struct MockTorrentBuilder {
    name: String,
    piece_length: u32,
    files: Vec<TorrentFile>,
    announce: Option<String>,
    is_single_file: bool,
    single_file_length: Option<u64>,
}

impl MockTorrentBuilder {
    /// Create a new torrent builder
    pub fn new(name: &str, piece_length: u32) -> Self {
        MockTorrentBuilder {
            name: name.to_string(),
            piece_length,
            files: Vec::new(),
            announce: None,
            is_single_file: true,
            single_file_length: None,
        }
    }

    /// Set as single file mode with specified length
    pub fn single_file(mut self, length: u64) -> Self {
        self.is_single_file = true;
        self.single_file_length = Some(length);
        self
    }

    /// Add a file (multi-file mode)
    pub fn add_file(mut self, path: &str, length: u64) -> Self {
        self.is_single_file = false;
        self.files.push(TorrentFile {
            length,
            path: path.split('/').map(|s| s.to_string()).collect(),
        });
        self
    }

    /// Set announce URL
    pub fn with_announce(mut self, url: &str) -> Self {
        self.announce = Some(url.to_string());
        self
    }

    /// Build the complete info dictionary in bencode format
    fn build_info_dict(&self) -> Vec<u8> {
        let mut info = Vec::new();
        info.push(b'd'); // dict start

        // Name
        info.extend_from_slice(&encode_bencode_string("name"));
        info.extend_from_slice(&encode_bencode_string(&self.name));

        // Piece length
        info.extend_from_slice(&encode_bencode_string("piece length"));
        info.extend_from_slice(&encode_bencode_int(self.piece_length as i64));

        if self.is_single_file {
            // Single file mode: use 'length' field
            if let Some(length) = self.single_file_length {
                info.extend_from_slice(&encode_bencode_string("length"));
                info.extend_from_slice(&encode_bencode_int(length as i64));
            }
        } else {
            // Multi-file mode: use 'files' list
            info.extend_from_slice(&encode_bencode_string("files"));
            info.push(b'l'); // list start

            for file in &self.files {
                info.push(b'd'); // file dict start

                // Length
                info.extend_from_slice(&encode_bencode_string("length"));
                info.extend_from_slice(&encode_bencode_int(file.length as i64));

                // Path (list of strings)
                info.extend_from_slice(&encode_bencode_string("path"));
                info.push(b'l');
                for path_component in &file.path {
                    info.extend_from_slice(&encode_bencode_string(path_component));
                }
                info.push(b'e'); // end path list

                info.push(b'e'); // end file dict
            }

            info.push(b'e'); // end files list
        }

        info.push(b'e'); // end info dict
        info
    }

    /// Calculate pieces hash based on total size and piece count
    fn calculate_pieces(&self, _total_size: u64, num_pieces: usize) -> Vec<u8> {
        let mut pieces = Vec::with_capacity(num_pieces * 20);

        for i in 0..num_pieces {
            // Create SHA-1 hash for each piece (using piece index as data)
            let mut hasher = Sha1::new();
            hasher.update(format!("piece_{}_{}", self.name, i));
            pieces.extend_from_slice(&hasher.finalize());
        }

        pieces
    }

    /// Build the complete .torrent file bytes in bencode format
    pub fn build_bytes(&self) -> Vec<u8> {
        let mut torrent = Vec::new();
        torrent.push(b'd'); // outer dict start

        // Announce URL (optional)
        if let Some(ref announce) = self.announce {
            torrent.extend_from_slice(&encode_bencode_string("announce"));
            torrent.extend_from_slice(&encode_bencode_string(announce));
        }

        // Info dictionary
        let info_dict = self.build_info_dict();

        // Calculate pieces hash based on total size
        let total_size = if self.is_single_file {
            self.single_file_length.unwrap_or(0)
        } else {
            self.files.iter().map(|f| f.length).sum()
        };

        let num_pieces = if total_size > 0 && self.piece_length > 0 {
            ((total_size + self.piece_length as u64 - 1) / self.piece_length as u64) as usize
        } else {
            1
        };

        let pieces = self.calculate_pieces(total_size, num_pieces);

        // Rebuild info dict with pieces field included
        let mut info_with_pieces = Vec::new();
        info_with_pieces.push(b'd');

        // Copy existing fields from info_dict (skip first and last byte which are d/e)
        if info_dict.len() > 2 {
            info_with_pieces.extend_from_slice(&info_dict[1..info_dict.len() - 1]);
        }

        // Add pieces field
        info_with_pieces.extend_from_slice(&encode_bencode_string("pieces"));
        info_with_pieces.extend_from_slice(&encode_bencode_bytes(&pieces));

        info_with_pieces.push(b'e'); // end info dict

        // Add to torrent
        torrent.extend_from_slice(&encode_bencode_string("info"));
        torrent.extend_from_slice(&info_with_pieces);

        torrent.push(b'e'); // end outer dict
        torrent
    }

    /// Calculate and return info_hash (SHA-1 of info dict)
    pub fn build_info_hash(&self) -> [u8; 20] {
        let info_dict = self.build_info_dict();

        // Recalculate with pieces for proper hash
        let bytes = self.build_bytes();

        // Extract info dict from the full torrent bytes
        // Find "info" key and extract the value
        let info_key = b"4:info";
        if let Some(pos) = bytes.windows(5).position(|w| w == info_key) {
            let info_start = pos + 5;
            // Parse the info dict to find its end
            if let Some(info_end) = find_bencode_end(&bytes[info_start..]) {
                let info_data = &bytes[info_start..info_start + info_end];
                let mut hasher = Sha1::new();
                hasher.update(info_data);
                return hasher.finalize().into();
            }
        }

        // Fallback: hash the entire info_dict we built
        let mut hasher = Sha1::new();
        hasher.update(&info_dict);
        hasher.finalize().into()
    }

    /// Build torrent with preset test data
    ///
    /// Returns: (torrent_bytes, expected_full_data)
    /// - completed_pieces pieces are filled with data_pattern bytes
    /// - remaining pieces are all zeros
    pub fn build_with_partial_data(
        self,
        total_pieces: usize,
        completed_pieces: usize,
        data_pattern: u8,
    ) -> (Vec<u8>, Vec<u8>) {
        let torrent_bytes = self.build_bytes();

        let total_size = total_pieces * self.piece_length as usize;
        let mut expected_data = vec![0u8; total_size];

        // Fill completed pieces with pattern
        for i in 0..completed_pieces.min(total_pieces) {
            let start = i * self.piece_length as usize;
            let end = (start + self.piece_length as usize).min(total_size);
            for j in start..end {
                expected_data[j] = data_pattern;
            }
        }

        (torrent_bytes, expected_data)
    }
}

/// Helper function to find the end of a bencode value
fn find_bencode_end(data: &[u8]) -> Option<usize> {
    if data.is_empty() {
        return None;
    }

    match data[0] {
        b'i' => {
            // Integer: i<digits>e
            data.iter().position(|&b| b == b'e').map(|p| p + 1)
        }
        b'l' | b'd' => {
            // List or Dict: need to count nested structures
            let mut depth = 1;
            let mut pos = 1;
            while pos < data.len() && depth > 0 {
                match data[pos] {
                    b'i' => {
                        // Skip to end of integer
                        if let Some(end) = data[pos..].iter().position(|&b| b == b'e') {
                            pos += end + 1;
                        } else {
                            return None;
                        }
                    }
                    b'l' | b'd' => {
                        depth += 1;
                        pos += 1;
                    }
                    b'e' => {
                        depth -= 1;
                        pos += 1;
                    }
                    b'0'..=b'9' => {
                        // String: len:data
                        let colon_pos = data[pos..].iter().position(|&b| b == b':')?;
                        let len_str = std::str::from_utf8(&data[pos..pos + colon_pos]).ok()?;
                        let len: usize = len_str.parse().ok()?;
                        pos += colon_pos + 1 + len;
                    }
                    _ => {
                        pos += 1;
                    }
                }
            }
            if depth == 0 { Some(pos) } else { None }
        }
        b'0'..=b'9' => {
            // String: len:data
            let colon_pos = data.iter().position(|&b| b == b':')?;
            let len_str = std::str::from_utf8(&data[..colon_pos]).ok()?;
            let len: usize = len_str.parse().ok()?;
            Some(colon_pos + 1 + len)
        }
        _ => None,
    }
}
