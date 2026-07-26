//! FTP response and directory listing parsers
//!
//! Contains all parsing logic for FTP protocol responses (PASV, EPSV)
//! and directory listing formats (Unix ls -l, Windows, MLSD).

use crate::error::{Aria2Error, Result};

use super::types::{FtpClient, FtpFileInfo};

impl FtpClient {
    /// Parse PASV response, extract IP address and port
    ///
    /// PASV response format: `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`
    ///
    /// # Arguments
    ///
    /// - `text`: Message portion of the PASV response
    ///
    /// # Returns
    ///
    /// Returns a `(host, port)` tuple
    pub(super) fn parse_pasv_response(text: &str) -> Result<(String, u16)> {
        let start = text.find('(').ok_or_else(|| {
            Aria2Error::Parse("PASV response missing opening parenthesis".to_string())
        })?;

        let end = text.find(')').ok_or_else(|| {
            Aria2Error::Parse("PASV response missing closing parenthesis".to_string())
        })?;

        let inner = &text[start + 1..end];
        let parts: Vec<&str> = inner.split(',').collect();

        if parts.len() != 6 {
            return Err(Aria2Error::Parse(format!(
                "PASV response format error: expected 6 parts, got {}",
                parts.len()
            )));
        }

        let h1: u8 = parts[0]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h1".to_string()))?;
        let h2: u8 = parts[1]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h2".to_string()))?;
        let h3: u8 = parts[2]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h3".to_string()))?;
        let h4: u8 = parts[3]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid IP byte h4".to_string()))?;
        let p1: u16 = parts[4]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid port byte p1".to_string()))?;
        let p2: u16 = parts[5]
            .trim()
            .parse()
            .map_err(|_| Aria2Error::Parse("PASV response: invalid port byte p2".to_string()))?;

        let host = format!("{}.{}.{}.{}", h1, h2, h3, h4);
        let port = p1 * 256 + p2;

        Ok((host, port))
    }

    /// Parse EPSV response, extract port number
    ///
    /// EPSV response format: `229 Entering Extended Passive Mode (|||port|)`
    ///
    /// # Arguments
    ///
    /// - `text`: Message portion of the EPSV response
    ///
    /// # Returns
    ///
    /// Returns the port number, or None if parsing fails
    pub(super) fn parse_epsv_response(text: &str) -> Option<u16> {
        let start = text.rfind('|')?;
        let prev_pipe = text[..start].rfind('|')?;
        let port_str = &text[prev_pipe + 1..start];
        port_str.parse::<u16>().ok()
    }

    /// Parse a single line of LIST output
    ///
    /// Supports Unix format (`-rw-r--r--  1 user group   size date  name`) and
    /// Windows format (`date       size  name` or `dir`).
    ///
    /// # Arguments
    ///
    /// - `line`: Single line of LIST output text
    ///
    /// # Returns
    ///
    /// Returns parsed file info, or None if parsing fails
    pub(crate) fn parse_list_line(line: &str) -> Option<FtpFileInfo> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Try Unix format parsing
        if let Some(info) = Self::parse_unix_list_line(trimmed) {
            return Some(info);
        }

        // Try Windows format parsing
        if let Some(info) = Self::parse_windows_list_line(trimmed) {
            return Some(info);
        }

        // Try MLSD format parsing
        if let Some(info) = Self::parse_mlsd_line(trimmed) {
            return Some(info);
        }

        None
    }

    /// Parse Unix ls -l format using fast path (zero-dependency string parsing)
    ///
    /// This fast path handles ~90% of real-world FTP LIST responses which use
    /// standard Unix ls -l format, avoiding regex compilation and matching overhead.
    ///
    /// Format: `[type][perms] [links] [owner] [group] [size] [mon] [day] [time/year] [name]`
    /// Example: `-rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf`
    ///
    /// # Returns
    ///
    /// `Some(FtpFileInfo)` if parsing succeeds, `None` if line doesn't match expected format
    fn parse_list_line_fast(line: &str) -> Option<FtpFileInfo> {
        // Minimum viable line length check:
        // type(1) + perms(9) + spaces(3+) + links(1+) + owner(1+) + spaces + group(1+)
        // + spaces + size(1+) + spaces + month(3) + spaces + day(1+) + spaces + time(4-5/year)
        // + space + name(1+) >= ~40 chars for realistic entries
        if line.len() < 35 {
            return None;
        }

        // Determine entry type from first character
        let entry_type = match line.as_bytes().first()? {
            b'd' => true,  // Directory
            b'-' => false, // Regular file
            b'l' => {
                // Symlink - handle specially below
                // For symlinks, we'll parse but mark as non-directory
                false
            }
            _ => return None, // Unknown type, fallback to regex
        };

        let is_dir = entry_type;

        // Validate permission field (chars 1-9 should be [rwxst-])
        let perms = &line[1..10];
        if !perms.chars().all(|c| "rwxst-".contains(c)) {
            return None;
        }

        // Skip permission field and split rest by whitespace
        let after_perms = line[10..].trim_start();

        // Find positions of each field by scanning for whitespace
        // Expected fields: links owner group size month day time/year name
        // We need to skip 7 fields and capture the rest as filename
        let mut pos = 0;
        for _ in 0..7 {
            // Skip current field (non-whitespace)
            let end = after_perms[pos..]
                .find(' ')
                .unwrap_or(after_perms.len() - pos);
            pos += end + 1;
            // Skip whitespace between fields
            while pos < after_perms.len() && after_perms.as_bytes()[pos] == b' ' {
                pos += 1;
            }
            if pos >= after_perms.len() {
                return None;
            }
        }

        // Remaining part is the filename (may contain spaces)
        let name_raw = after_perms[pos..].trim();
        if name_raw.is_empty() {
            return None;
        }

        // Handle symlink format: "linkname -> target"
        let actual_name = if line.as_bytes()[0] == b'l' {
            if let Some(arrow_pos) = name_raw.find(" -> ") {
                &name_raw[..arrow_pos]
            } else {
                name_raw
            }
        } else {
            name_raw
        };

        // Filter out special entries
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        // Parse size from the line (field index 3, 0-based)
        // Fields after permissions: links(0) owner(1) group(2) size(3) month(4) ...
        let size_field = after_perms.split_whitespace().nth(3)?;
        let size: u64 = size_field.parse().ok()?;

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// Parse Unix-format LIST line with fast path optimization
    ///
    /// Tries zero-allocation string parsing first (~90% of cases),
    /// falls back to regex for exotic formats.
    fn parse_unix_list_line(line: &str) -> Option<FtpFileInfo> {
        // Fast path for standard Unix ls -l format (avoids regex overhead)
        if let Some(info) = Self::parse_list_line_fast(line) {
            return Some(info);
        }

        // Fallback to regex for exotic/non-standard formats
        Self::parse_unix_list_line_regex(line)
    }

    /// Parse Unix ls -l format using regex (fallback for non-standard formats)
    ///
    /// Unix format example:
    /// ```text
    /// -rw-r--r--  1 user group  12345 Jan 15 10:30 filename.txt
    /// drwxr-xr-x  2 user group   4096 Feb  3 14:20 directory
    /// lrwxrwxrwx  1 user group     8 Mar 10 09:00 link -> target
    /// ```
    fn parse_unix_list_line_regex(line: &str) -> Option<FtpFileInfo> {
        // Use regex to match Unix ls -l format
        // Format: [type][perms]  [links] [user] [group] [size] [mon] [day] [time/year] [name]
        // Example: -rw-r--r--  1 user staff  12345 Jan 15 10:30 document.pdf

        // Regex pattern explanation:
        // ^([bcdlsp-])           # File type (1 char)
        // ([rwxst-]{9})          # Permission bits (9 chars)
        // \s+                     # One or more spaces
        // (\d+)                   # Hard link count
        // \s+                     # Space
        // (\S+)                   # Username
        // \s+                     # Space
        // (\S+)                   # Group name
        // \s+                     # Space
        // (\d+)                   # File size
        // \s+                     # Space
        // (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)  # Month
        // \s+                     # Space
        // (\d{1,2})              # Day (1-2 digits)
        // \s+                     # Space
        // (\d{4}|\d{1,2}:\d{2})  # Year (4 digits) or time (HH:MM)
        // \s+                     # Space
        // (.+)$                  # Filename (may contain spaces)

        use regex::Regex;

        let re = Regex::new(
            r"^([bcdlsp-])([rwxst-]{9})\s+(\d+)\s+(\S+)\s+(\S+)\s+(\d+)\s+(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+(\d{1,2})\s+(\d{4}|\d{1,2}:\d{2})\s+(.+)$"
        ).ok()?;

        let caps = re.captures(line)?;

        let type_char = caps.get(1)?.as_str().chars().next()?;
        let is_dir = type_char == 'd';
        let is_link = type_char == 'l';

        let size: u64 = caps.get(6)?.as_str().parse().ok()?;
        let name = caps.get(10)?.as_str();

        if name.is_empty() {
            return None;
        }

        // Handle symlink: "link -> target"
        let actual_name = if is_link {
            if let Some(arrow_pos) = name.find(" -> ") {
                &name[..arrow_pos]
            } else {
                name
            }
        } else {
            name
        };

        // Special entries: "." and ".."
        if actual_name == "." || actual_name == ".." {
            return None;
        }

        Some(FtpFileInfo {
            name: actual_name.to_string(),
            size,
            is_dir,
        })
    }

    /// Parse Windows/DOS format LIST line
    ///
    /// Windows format example:
    /// ```text
    /// 01-15-24  10:30AM    12345 filename.txt
    /// 02-03-24  02:20PM    <DIR> directory
    /// ```
    fn parse_windows_list_line(line: &str) -> Option<FtpFileInfo> {
        // Windows format: "MM-DD-YY  HH:MM[AP]M  <DIR>/size  name"
        // Minimum length check
        if line.len() < 20 {
            return None;
        }

        // Date part: MM-DD-YY (8 characters)
        let date_part = &line[..8];
        if date_part.len() != 8
            || date_part.chars().nth(2)? != '-'
            || date_part.chars().nth(5)? != '-'
        {
            return None;
        }

        let after_date = line[8..].trim_start();

        // Time part: HH:MM[AP]M (7-9 characters)
        let space_pos = after_date.find(' ')?;
        let time_part = &after_date[..space_pos];
        if !time_part.contains(':') {
            return None;
        }

        let after_time = after_date[space_pos + 1..].trim_start();

        // Size or <DIR>
        let space_pos = after_time.find(' ')?;
        let size_or_dir = after_time[..space_pos].trim();

        let is_dir = size_or_dir.eq_ignore_ascii_case("<DIR>");
        let size: u64 = if is_dir { 0 } else { size_or_dir.parse().ok()? };

        // Filename
        let name = after_time[space_pos + 1..].trim().to_string();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        Some(FtpFileInfo { name, size, is_dir })
    }

    /// Parse MLSD (Machine Listing) format line
    ///
    /// MLSD format example:
    /// ```text
    /// type=file;size=12345;modify=20240115103000;unix.mode=0644; filename.txt
    /// type=dir;size=4096;modify=20240203142000;unix.mode=0755; directory
    /// type=os.unix=symlink=/target;size=8; link
    /// ```
    fn parse_mlsd_line(line: &str) -> Option<FtpFileInfo> {
        // MLSD format: facts; facts; ... name
        // Facts and name are separated by a space
        let semicolon_pos = line.rfind("; ")?;
        let (facts_str, name) = line.split_at(semicolon_pos + 2);
        let name = name.trim();

        if name.is_empty() || name == "." || name == ".." {
            return None;
        }

        // Parse facts
        let mut is_dir = false;
        let mut size: u64 = 0;

        for fact in facts_str.split(';') {
            let fact = fact.trim();
            if fact.is_empty() {
                continue;
            }

            if let Some(eq_pos) = fact.find('=') {
                let key = &fact[..eq_pos];
                let value = &fact[eq_pos + 1..];

                match key.to_lowercase().as_str() {
                    "type" => {
                        is_dir = value.eq_ignore_ascii_case("dir")
                            || value.eq_ignore_ascii_case("cdir")
                            || value.eq_ignore_ascii_case("pdir");
                    }
                    "size" => {
                        size = value.parse().unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }

        Some(FtpFileInfo {
            name: name.to_string(),
            size,
            is_dir,
        })
    }
}
