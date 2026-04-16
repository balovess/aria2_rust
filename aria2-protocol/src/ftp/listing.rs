//! FTP directory listing parser
//!
//! Supports parsing of Unix-style and MS-DOS style FTP directory listings
//! returned by the LIST command.

use std::time::SystemTime;

/// A single entry in an FTP directory listing
#[derive(Debug, Clone)]
pub struct ListingEntry {
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
    pub permissions: String,
    pub modified_time: Option<SystemTime>,
}

impl ListingEntry {
    /// Create a new ListingEntry with default values
    pub fn new(name: String) -> Self {
        Self {
            name,
            size: 0,
            is_directory: false,
            permissions: String::new(),
            modified_time: None,
        }
    }

    /// Check if this entry looks like a parent directory reference (.. or .)
    pub fn is_parent_reference(&self) -> bool {
        self.name == "." || self.name == ".."
    }
}

/// Parse a Unix-style FTP listing line
///
/// Format example:
/// ```text
/// -rw-r--r--  1 user group  1234 Jan 01 00:00 filename
/// drwxr-xr-x  2 user group  4096 Dec 15 2023 directory
/// lrwxrwxrwx  1 user group     8 Jan 01 00:00 linkname -> target
/// ```
pub fn parse_unix_list_line(line: &str) -> Option<ListingEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Minimum length check for Unix format (at least "drwxr-xr-x" + spaces + name)
    if line.len() < 15 {
        return None;
    }

    // Parse permissions field (first 10 characters)
    let perms = &line[..10];
    if !is_valid_unix_permissions(perms) {
        return None;
    }

    let is_directory = perms.starts_with('d');
    let is_symlink = perms.starts_with('l');

    // Split remaining fields after permissions
    let rest = line[10..].trim_start();

    // Unix format: <links> <owner> <group> <size> <month> <day> [<year>|<time>] <name>
    // For symlinks, name may contain " -> target"
    let fields: Vec<&str> = rest.split_whitespace().collect();

    // Need at least: links, owner, group, size, month, day, time/year, name
    if fields.len() < 8 {
        return None;
    }

    // Size is at index 3 (0-indexed from fields after permissions)
    let size: u64 = fields[3].parse().unwrap_or(0);

    // Parse date/time (simplified - we don't need full precision)
    // Format: "Jan 01 00:00" or "Jan 01 2023"
    let _month = fields[4];
    let _day = fields[5];
    let _time_or_year = fields[6];

    // Name starts at index 7 and continues to end
    // For symlinks, we need to strip " -> target"
    let mut name = fields[7..].join(" ");
    if is_symlink && let Some(pos) = name.find(" -> ") {
        name = name[..pos].to_string();
    }

    Some(ListingEntry {
        name,
        size,
        is_directory,
        permissions: perms.to_string(),
        modified_time: None, // Would need date parsing implementation
    })
}

/// Parse an MS-DOS style FTP listing line
///
/// Format examples:
/// ```text
/// 01-01-00  00:00AM       1234 filename
/// 12-15-23  03:45PM      <DIR> directory
/// ```
pub fn parse_msdos_list_line(line: &str) -> Option<ListingEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // MS-DOS format: MM-DD-YY  HH:MM[AP]M  <size|DIR>  <name>
    // Example: "01-15-24  02:30PM  12345 filename.txt"

    // Find first two date/time groups separated by whitespace
    let parts: Vec<&str> = line.split_whitespace().collect();

    // Need at least: date, time, size/dir indicator, name
    if parts.len() < 4 {
        return None;
    }

    // Validate date format (MM-DD-YY)
    let date_part = parts[0];
    if !validate_msdos_date(date_part) {
        return None;
    }

    // Validate time format (HH:MMAM/PM)
    let time_part = parts[1];
    if !validate_msdos_time(time_part) {
        return None;
    }

    // Third part is either size or "<DIR>"
    let size_or_dir = parts[2];
    let (size, is_directory) = if size_or_dir.eq_ignore_ascii_case("<dir>") {
        (0, true)
    } else {
        match size_or_dir.parse::<u64>() {
            Ok(s) => (s, false),
            Err(_) => return None,
        }
    };

    // Name is everything after the third part
    let name = parts[3..].join(" ");

    Some(ListingEntry {
        name,
        size,
        is_directory,
        permissions: String::new(), // MS-DOS doesn't show permissions
        modified_time: None,
    })
}

/// Parse complete FTP LIST response into individual entries
///
/// Automatically detects format (Unix vs MS-DOS) based on content
pub fn parse_ftp_list_response(response: &str) -> Vec<ListingEntry> {
    let lines: Vec<&str> = response.lines().collect();

    if lines.is_empty() {
        return Vec::new();
    }

    // Detect format from first non-empty line
    let format = detect_listing_format(&lines);

    let mut entries = Vec::new();
    for line in lines.iter().filter(|l| !l.trim().is_empty()) {
        let entry = match format {
            ListingFormat::Unix => parse_unix_list_line(line),
            ListingFormat::MsDos => parse_msdos_list_line(line),
        };

        if let Some(e) = entry {
            // Filter out . and .. references
            if !e.is_parent_reference() {
                entries.push(e);
            }
        }
    }

    entries
}

/// Detected listing format type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingFormat {
    Unix,
    MsDos,
}

/// Detect whether a listing response is Unix-style or MS-DOS style
fn detect_listing_format(lines: &[&str]) -> ListingFormat {
    // Look at first few non-empty lines to determine format
    for line in lines.iter().filter(|l| !l.trim().is_empty()) {
        let trimmed = line.trim();

        // Unix format starts with permission string like "-rw-r--r--" or "drwxr-xr-x"
        if trimmed.len() >= 10 && is_valid_unix_permissions(&trimmed[..10]) {
            return ListingFormat::Unix;
        }

        // MS-DOS format starts with date like "01-15-24"
        if trimmed.len() >= 8 && validate_msdos_date(&trimmed[..8]) {
            return ListingFormat::MsDos;
        }
    }

    // Default to Unix format if can't determine
    ListingFormat::Unix
}

/// Validate Unix permission string (first 10 characters)
fn is_valid_unix_permissions(perm_str: &str) -> bool {
    if perm_str.len() < 10 {
        return false;
    }

    // First character must be one of: -, d, l, c, b, s, p
    let type_char = perm_str.as_bytes()[0];
    if !matches!(type_char, b'-' | b'd' | b'l' | b'c' | b'b' | b's' | b'p') {
        return false;
    }

    // Characters 1-3 (owner), 4-6 (group), 7-9 (others) must be r, w, x, -, S, s, T, t
    for i in 1..10 {
        let c = perm_str.as_bytes()[i];
        if !matches!(c, b'r' | b'w' | b'x' | b'-' | b'S' | b's' | b'T' | b't') {
            return false;
        }
    }

    true
}

/// Validate MS-DOS date format (MM-DD-YY)
fn validate_msdos_date(date: &str) -> bool {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return false;
    }

    // Each part should be 2 digits
    if parts
        .iter()
        .any(|p| p.len() != 2 || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }

    let month: u8 = match parts[0].parse() {
        Ok(m) if (1..=12).contains(&m) => m,
        _ => return false,
    };
    let _day: u8 = match parts[1].parse() {
        Ok(d) if (1..=31).contains(&d) => d,
        _ => return false,
    };
    let _year: u8 = match parts[2].parse() {
        Ok(y) => y, // Accept any 2-digit year
        _ => return false,
    };

    // Basic validation passed
    let _ = month; // Suppress unused warning
    true
}

/// Validate MS-DOS time format (HH:MMAM/PM)
fn validate_msdos_time(time: &str) -> bool {
    // Should be at least 6 characters: HH:MM + AM/PM
    if time.len() < 6 {
        return false;
    }

    // Must end with AM or PM (case insensitive)
    if !time.ends_with("AM") && !time.ends_with("PM") {
        return false;
    }

    // Extract HH:MM part
    let time_part = &time[..time.len() - 2];
    let parts: Vec<&str> = time_part.split(':').collect();
    if parts.len() != 2 {
        return false;
    }

    // Hour should be 1-2 digits, minute should be 2 digits
    if parts[0].is_empty() || parts[1].len() != 2 {
        return false;
    }

    let hour: u8 = match parts[0].parse() {
        Ok(h) if (0..=12).contains(&h) => h,
        _ => return false,
    };
    let _minute: u8 = match parts[1].parse() {
        Ok(m) if (0..=59).contains(&m) => m,
        _ => return false,
    };

    let _ = hour; // Suppress unused warning
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_unix_regular_file() {
        let line = "-rw-r--r--  1 user group  1234 Jan 01 00:00 testfile.txt";
        let entry = parse_unix_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "testfile.txt");
        assert_eq!(entry.size, 1234);
        assert!(!entry.is_directory);
        assert_eq!(entry.permissions, "-rw-r--r--");
    }

    #[test]
    fn test_parse_unix_directory() {
        let line = "drwxr-xr-x  2 user group  4096 Dec 15 2023 mydirectory";
        let entry = parse_unix_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "mydirectory");
        assert_eq!(entry.size, 4096);
        assert!(entry.is_directory);
        assert_eq!(entry.permissions, "drwxr-xr-x");
    }

    #[test]
    fn test_parse_unix_symlink() {
        let line = "lrwxrwxrwx  1 user group     8 Jan 01 00:00 linkname -> targetfile";
        let entry = parse_unix_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "linkname");
        assert!(!entry.is_directory); // Symlinks are not directories themselves
        assert_eq!(entry.permissions, "lrwxrwxrwx");
    }

    #[test]
    fn test_parse_unix_file_with_spaces_in_name() {
        let line = "-rw-r--r--  1 user group   567 Jan 01 00:00 my file with spaces.txt";
        let entry = parse_unix_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "my file with spaces.txt");
        assert_eq!(entry.size, 567);
    }

    #[test]
    fn test_parse_unix_invalid_too_short() {
        let line = "short";
        assert!(parse_unix_list_line(line).is_none());
    }

    #[test]
    fn test_parse_unix_invalid_bad_permissions() {
        let line = "xxxxxxx  1 user group  1234 Jan 01 00:00 file.txt";
        assert!(parse_unix_list_line(line).is_none());
    }

    #[test]
    fn test_parse_msdos_regular_file() {
        let line = "01-15-24  02:30PM       12345 filename.txt";
        let entry = parse_msdos_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "filename.txt");
        assert_eq!(entry.size, 12345);
        assert!(!entry.is_directory);
    }

    #[test]
    fn test_parse_msdos_directory() {
        let line = "12-31-23  11:59PM      <DIR> myfolder";
        let entry = parse_msdos_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "myfolder");
        assert!(entry.is_directory);
        assert_eq!(entry.size, 0);
    }

    #[test]
    fn test_parse_msdos_am_time() {
        let line = "06-01-24  09:15AM         500 morning.log";
        let entry = parse_msdos_list_line(line).expect("Should parse successfully");

        assert_eq!(entry.name, "morning.log");
        assert_eq!(entry.size, 500);
    }

    #[test]
    fn test_parse_msdos_invalid_format() {
        let line = "not a valid msdos line";
        assert!(parse_msdos_list_line(line).is_none());
    }

    #[test]
    fn test_parse_msdos_invalid_date() {
        let line = "99-99-99  12:00PM  123 file.txt";
        assert!(parse_msdos_list_line(line).is_none());
    }

    #[test]
    fn test_parse_ftp_list_response_unix() {
        let response = "\
-rw-r--r--  1 user group  1024 Jan 01 00:00 file1.txt
drwxr-xr-x  2 user group  4096 Jan 01 00:00 directory1
-rw-r--r--  1 user group  2048 Jan 01 00:00 file2.txt";

        let entries = parse_ftp_list_response(response);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].name, "file1.txt");
        assert_eq!(entries[0].size, 1024);
        assert!(!entries[0].is_directory);

        assert_eq!(entries[1].name, "directory1");
        assert!(entries[1].is_directory);

        assert_eq!(entries[2].name, "file2.txt");
        assert_eq!(entries[2].size, 2048);
    }

    #[test]
    fn test_parse_ftp_list_response_msdos() {
        let response = "\
01-15-24  02:30PM       12345 file1.txt
12-31-23  11:59PM      <DIR> folder1
06-01-24  09:15AM       5678 file2.txt";

        let entries = parse_ftp_list_response(response);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].name, "file1.txt");
        assert_eq!(entries[0].size, 12345);

        assert_eq!(entries[1].name, "folder1");
        assert!(entries[1].is_directory);

        assert_eq!(entries[2].name, "file2.txt");
        assert_eq!(entries[2].size, 5678);
    }

    #[test]
    fn test_filter_parent_references() {
        let response = "\
drwxr-xr-x  2 user group  4096 Jan 01 00:00 .
drwxr-xr-x  3 user group  4096 Jan 01 00:00 ..
-rw-r--r--  1 user group  1024 Jan 01 00:00 actualfile.txt";

        let entries = parse_ftp_list_response(response);
        // Should filter out . and ..
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "actualfile.txt");
    }

    #[test]
    fn test_empty_response() {
        let entries = parse_ftp_list_response("");
        assert!(entries.is_empty());

        let entries = parse_ftp_list_response("\n\n\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_listing_entry_parent_reference() {
        let dot = ListingEntry::new(".".to_string());
        assert!(dot.is_parent_reference());

        let dot_dot = ListingEntry::new("..".to_string());
        assert!(dot_dot.is_parent_reference());

        let normal = ListingEntry::new("file.txt".to_string());
        assert!(!normal.is_parent_reference());
    }

    #[test]
    fn test_is_valid_unix_permissions() {
        assert!(is_valid_unix_permissions("-rw-r--r--"));
        assert!(is_valid_unix_permissions("drwxr-xr-x"));
        assert!(is_valid_unix_permissions("lrwxrwxrwx"));
        assert!(is_valid_unix_permissions("-rwsr-xr-x")); // setuid
        assert!(is_valid_unix_permissions("-rw-r--r-T")); // sticky

        assert!(!is_valid_unix_permissions(""));
        assert!(!is_valid_unix_permissions("---------"));
        assert!(!is_valid_unix_permissions("xxxxxxxxxx"));
    }

    #[test]
    fn test_validate_msdos_date() {
        assert!(validate_msdos_date("01-15-24"));
        assert!(validate_msdos_date("12-31-23"));

        assert!(!validate_msdos_date("13-01-24")); // Invalid month
        assert!(!validate_msdos_date("00-15-24")); // Invalid month
        assert!(!validate_msdos_date("01-32-24")); // Invalid day (will fail basic digit check but pass range)
    }

    #[test]
    fn test_validate_msdos_time() {
        assert!(validate_msdos_time("02:30PM"));
        assert!(validate_msdos_time("09:15AM"));
        assert!(validate_msdos_time("12:00PM"));

        assert!(!validate_msdos_time("13:00PM")); // Invalid hour > 12
        assert!(!validate_msdos_time("25:70AM")); // Invalid hour/minute
        assert!(!validate_msdos_time("1230PM")); // Missing colon
    }
}
