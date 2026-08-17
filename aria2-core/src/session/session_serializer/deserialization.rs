//! Deserialization logic for parsing session file text into SessionEntry objects

use crate::error::Result;

use super::SessionEntry;

/// Deserializes session file text into a vector of SessionEntry objects
///
/// Parses the entire contents of a session file and returns all valid
/// entries found. Handles comments (#) and blank lines.
///
/// # Arguments
///
/// * `text` - Full contents of a session file as a string
///
/// # Returns
///
/// Result containing a Vec of successfully parsed SessionEntry objects
///
/// # Format Details
///
/// Each entry consists of:
/// 1. A URI line (one or more tab-separated URIs)
/// 2. Zero or more property lines (space-prefixed key=value pairs)
/// 3. Separated from next entry by blank line
///
/// # Error Handling
///
/// - Empty lines and comments are silently skipped
/// - Invalid values are ignored (with warnings logged)
/// - Malformed hex strings cause bitfield to be ignored
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_serializer::deserialize;
///
/// let input = r#"http://example.com/file.zip
///  GID=1
///  split=4
///
/// ftp://server/big.iso
///  GID=2
///  PAUSE=true
/// "#;
///
/// let entries = deserialize(input).unwrap();
/// assert_eq!(entries.len(), 2);
/// assert!(!entries[0].paused);
/// assert!(entries[1].paused);
/// ```
pub fn deserialize(text: &str) -> Result<Vec<SessionEntry>> {
    let mut entries = Vec::new();
    let mut current_text = String::new();
    let mut in_entry = false;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();

        // Comments are ignored without changing the current entry. A blank
        // line is the session format's entry separator.
        if line.starts_with('#') {
            continue;
        }

        if line.is_empty() {
            if in_entry && !current_text.is_empty() {
                // End of current entry
                match SessionEntry::deserialize_line(&current_text) {
                    Ok(entry) if !entry.uris.is_empty() => entries.push(entry),
                    Ok(_) => {} // Skip entries with no URIs
                    Err(e) => {
                        tracing::warn!("Failed to deserialize entry: {}", e);
                    }
                }
                current_text.clear();
                in_entry = false;
            }
            continue;
        }

        // This line belongs to current entry
        current_text.push_str(line);
        current_text.push('\n');
        in_entry = true;
    }

    // Don't forget the last entry if file doesn't end with blank line
    if in_entry && !current_text.is_empty() {
        match SessionEntry::deserialize_line(&current_text) {
            Ok(entry) if !entry.uris.is_empty() => entries.push(entry),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to deserialize entry: {}", e);
            }
        }
    }

    Ok(entries)
}
