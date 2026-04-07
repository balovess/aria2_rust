use std::fmt;

#[derive(Debug, Clone)]
pub struct UriListEntry {
    pub uris: Vec<String>,
    pub options: std::collections::HashMap<String, String>,
}

impl UriListEntry {
    pub fn new(uris: Vec<String>) -> Self {
        Self { uris, options: std::collections::HashMap::new() }
    }

    pub fn with_options(mut self, options: std::collections::HashMap<String, String>) -> Self {
        self.options = options;
        self
    }

    pub fn is_valid(&self) -> bool { !self.uris.is_empty() }

    pub fn primary_uri(&self) -> Option<&String> { self.uris.first() }

    pub fn option(&self, key: &str) -> Option<&String> { self.options.get(key) }
}

#[derive(Debug, Clone)]
pub struct UriListFile {
    entries: Vec<UriListEntry>,
    path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum UriListError {
    FileNotFound(String),
    ParseError(usize, String),
    IoError(String),
}

impl fmt::Display for UriListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotFound(p) => write!(f, "URI list file not found: {}", p),
            Self::ParseError(line, msg) => write!(f, "parse error at line {}: {}", line, msg),
            Self::IoError(e) => write!(f, "io error: {}", e),
        }
    }
}

impl std::error::Error for UriListError {}

impl UriListFile {
    pub fn new() -> Self {
        Self { entries: Vec::new(), path: None }
    }

    pub fn from_file(path: &str) -> Result<Self, UriListError> {
        let p = std::path::Path::new(path);
        if !p.exists() { return Err(UriListError::FileNotFound(path.to_string())); }
        let content = std::fs::read_to_string(path)
            .map_err(|e| UriListError::IoError(format!("{}: {}", path, e)))?;
        let mut parser = Self::new();
        parser.path = Some(path.to_string());
        parser.parse(&content)?;
        Ok(parser)
    }

    pub fn parse(&mut self, content: &str) -> Result<(), UriListError> {
        let mut current_uris: Vec<String> = Vec::new();
        let mut current_options: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        for raw_line in content.lines() {
            let line = raw_line.trim();

            if line.is_empty() || line.starts_with('#') {
                if line.is_empty() && !current_uris.is_empty() {
                    self.entries.push(UriListEntry {
                        uris: std::mem::take(&mut current_uris),
                        options: std::mem::take(&mut current_options),
                    });
                } else if line.is_empty() {
                    current_options = std::collections::HashMap::new();
                }
                continue;
            }

            if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
                let opt_line = line.trim();
                if let Some(eq_pos) = opt_line.find('=') {
                    let opt_name = opt_line[..eq_pos].trim().to_string();
                    let opt_value = opt_line[eq_pos+1..].trim().to_string();
                    current_options.insert(opt_name, opt_value);
                }
                continue;
            }

            if !current_uris.is_empty() {
                self.entries.push(UriListEntry {
                    uris: std::mem::take(&mut current_uris),
                    options: std::mem::take(&mut current_options),
                });
            }

            let uris: Vec<String> = line
                .split('\t')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();

            if !uris.is_empty() {
                current_uris = uris;
            }
        }

        if !current_uris.is_empty() {
            self.entries.push(UriListEntry {
                uris: current_uris,
                options: current_options,
            });
        }

        Ok(())
    }

    pub fn entries(&self) -> &[UriListEntry] { &self.entries }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
    pub fn path(&self) -> Option<&str> { self.path.as_deref() }

    pub fn all_uris(&self) -> Vec<&str> {
        self.entries.iter().flat_map(|e| e.uris.iter().map(|s| s.as_str())).collect()
    }

    pub fn valid_entries(&self) -> Vec<&UriListEntry> {
        self.entries.iter().filter(|e| e.is_valid()).collect()
    }

    pub fn filter_by_option(&self, key: &str, value: &str) -> Vec<&UriListEntry> {
        self.entries.iter()
            .filter(|e| e.option(key).map_or(false, |v| v == value))
            .collect()
    }
}

impl Default for UriListFile {
    fn default() -> Self { Self::new() }
}

pub fn parse_uri_list(content: &str) -> Result<Vec<UriListEntry>, UriListError> {
    let mut file = UriListFile::new();
    file.parse(content)?;
    Ok(file.entries)
}

pub fn parse_single_line(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') { return None; }
    let uris: Vec<String> = trimmed
        .split('\t')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if uris.is_empty() { None } else { Some(uris) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_uris() {
        let content = "http://example.com/file1.iso\nhttp://example.com/file2.zip\n";
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 2);
        assert_eq!(file.entries()[0].uris.len(), 1);
        assert_eq!(file.entries()[0].primary_uri().unwrap(), "http://example.com/file1.iso");
    }

    #[test]
    fn test_parse_mirrors_on_same_line() {
        let content = "http://mirror1.com/file.iso\thttp://mirror2.com/file.iso\thttp://mirror3.com/file.iso\n";
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 1);
        assert_eq!(file.entries()[0].uris.len(), 3);
    }

    #[test]
    fn test_parse_inline_options() {
        let content = r#"  dir=/downloads
  out=myfile.iso
http://example.com/large.bin
"#;
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 1);
        assert_eq!(file.entries()[0].option("dir"), Some(&"/downloads".to_string()));
        assert_eq!(file.entries()[0].option("out"), Some(&"myfile.iso".to_string()));
    }

    #[test]
    fn test_parse_multiple_entries_with_options() {
        let content = r#"  dir=/tmp
  out=file1.dat
http://example.com/a.dat

  dir=/opt
  out=file2.dat
http://example.com/b.dat
"#;
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 2);
        assert_eq!(file.entries()[0].option("dir").map(|s| s.as_str()), Some("/tmp"));
        assert_eq!(file.entries()[1].option("dir").map(|s| s.as_str()), Some("/opt"));
    }

    #[test]
    fn test_skip_comments_and_blanks() {
        let content = r#"# Download list
# Generated by aria2

http://example.com/valid.iso

# This is ignored
http://example.com/another.iso
"#;
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 2);
    }

    #[test]
    fn test_empty_file() {
        let mut file = UriListFile::new();
        file.parse("").unwrap();
        assert!(file.is_empty());
        assert_eq!(file.len(), 0);
    }

    #[test]
    fn test_all_uris_collection() {
        let content = "http://a.com/f1\nhttp://b.com/f2\thttp://c.com/f2\n";
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        let all = file.all_uris();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_valid_entries_filter() {
        let content = "\n  dir=/empty\n\n";
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert!(file.valid_entries().is_empty());
    }

    #[test]
    fn test_filter_by_option() {
        let content = r#"  category=video
http://example.com/video.mp4

  category=audio
http://example.com/audio.mp3
"#;
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        let videos = file.filter_by_option("category", "video");
        assert_eq!(videos.len(), 1);
    }

    #[test]
    fn test_parse_uri_list_function() {
        let content = "http://example.com/file.iso\n";
        let entries = parse_uri_list(content).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_parse_single_line_function() {
        let uris = parse_single_line("http://a.com/f\thttp://b.com/f");
        assert!(uris.is_some());
        assert_eq!(uris.unwrap().len(), 2);

        assert!(parse_single_line("# comment").is_none());
        assert!(parse_single_line("").is_none());
    }

    #[test]
    fn test_entry_is_valid() {
        let valid = UriListEntry::new(vec!["http://x.com".into()]);
        assert!(valid.is_valid());

        let invalid = UriListEntry::new(vec![]);
        assert!(!invalid.is_valid());
    }

    #[test]
    fn test_entry_primary_uri() {
        let entry = UriListEntry::new(vec!["http://first.com".into(), "http://second.com".into()]);
        assert_eq!(entry.primary_uri().unwrap(), "http://first.com");
        assert!(entry.primary_uri().is_some());
    }

    #[test]
    fn test_error_display() {
        let err = UriListError::FileNotFound("/missing.txt".into());
        assert!(err.to_string().contains("not found"));

        let err2 = UriListError::ParseError(42, "bad uri".into());
        assert!(err2.to_string().contains("line 42"));
    }

    #[test]
    fn test_complex_realistic_input() {
        let content = r#"# My download list
  dir=D:\Downloads
  split=5
  max-connection-per-server=3
https://releases.ubuntu.com/22.04/ubuntu-22.04-desktop-amd64.iso	https://mirrors.tuna.tsinghua.edu.cn/ubuntu-releases/22.04/ubuntu-22.04-desktop-amd64.iso

  dir=D:\Downloads\Music
  out=song.mp3
https://example.com/music/song.mp3
"#;
        let mut file = UriListFile::new();
        file.parse(content).unwrap();
        assert_eq!(file.len(), 2);
        assert_eq!(file.entries()[0].uris.len(), 2);
        assert_eq!(file.entries()[0].option("split").map(|s| s.as_str()), Some("5"));
        assert_eq!(file.entries()[1].option("out").map(|s| s.as_str()), Some("song.mp3"));
    }
}
