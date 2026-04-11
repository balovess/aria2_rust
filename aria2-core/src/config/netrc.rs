use std::collections::HashMap;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct NetRcEntry {
    pub machine: String,
    pub login: Option<String>,
    pub password: Option<String>,
    pub account: Option<String>,
    pub macdef: Option<String>,
}

impl NetRcEntry {
    pub fn new(machine: impl Into<String>) -> Self {
        Self {
            machine: machine.into(),
            login: None,
            password: None,
            account: None,
            macdef: None,
        }
    }

    pub fn has_credentials(&self) -> bool {
        self.login.is_some() && self.password.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct NetRcFile {
    entries: Vec<NetRcEntry>,
    default_entry: Option<NetRcEntry>,
    path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum NetRcError {
    FileNotFound(String),
    ParseError(String),
    IoError(String),
}

impl fmt::Display for NetRcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileNotFound(p) => write!(f, "netrc file not found: {}", p),
            Self::ParseError(e) => write!(f, "netrc parse error: {}", e),
            Self::IoError(e) => write!(f, "netrc io error: {}", e),
        }
    }
}

impl std::error::Error for NetRcError {}

impl NetRcFile {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            default_entry: None,
            path: None,
        }
    }

    pub fn from_file(path: &str) -> Result<Self, NetRcError> {
        let p = Path::new(path);
        if !p.exists() {
            return Err(NetRcError::FileNotFound(path.to_string()));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| NetRcError::IoError(format!("{}: {}", path, e)))?;
        let mut parser = Self::new();
        parser.path = Some(path.to_string());
        parser.parse(&content)?;
        Ok(parser)
    }

    pub fn parse(&mut self, content: &str) -> Result<(), NetRcError> {
        let mut current_entry: Option<NetRcEntry> = None;
        let mut in_macdef = false;
        let mut macdef_name: Option<String> = None;
        let mut macdef_lines: Vec<String> = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if in_macdef {
                if line.is_empty() {
                    in_macdef = false;
                    if let (Some(entry), Some(_name)) = (&mut current_entry, macdef_name.take()) {
                        entry.macdef = Some(macdef_lines.join("\n"));
                        macdef_lines.clear();
                    }
                } else {
                    macdef_lines.push(line.to_string());
                }
                continue;
            }

            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.is_empty() {
                continue;
            }

            match tokens[0].to_lowercase().as_str() {
                "machine" => {
                    if let Some(prev) = current_entry.take() {
                        if prev.machine == "default" {
                            self.default_entry = Some(prev);
                        } else {
                            self.entries.push(prev);
                        }
                    }
                    let machine_name = if tokens.len() > 1 {
                        tokens[1].to_string()
                    } else {
                        String::new()
                    };
                    current_entry = Some(NetRcEntry::new(machine_name));
                }
                "default" => {
                    if let Some(prev) = current_entry.take() {
                        self.entries.push(prev);
                    }
                    current_entry = Some(NetRcEntry::new("default".to_string()));
                }
                "login" => {
                    if let Some(ref mut entry) = current_entry {
                        entry.login = if tokens.len() > 1 {
                            Some(tokens[1].to_string())
                        } else {
                            None
                        };
                    }
                }
                "password" | "passwd" => {
                    if let Some(ref mut entry) = current_entry {
                        entry.password = if tokens.len() > 1 {
                            Some(tokens[1].to_string())
                        } else {
                            None
                        };
                    }
                }
                "account" => {
                    if let Some(ref mut entry) = current_entry {
                        entry.account = if tokens.len() > 1 {
                            Some(tokens[1].to_string())
                        } else {
                            None
                        };
                    }
                }
                "macdef" => {
                    macdef_name = if tokens.len() > 1 {
                        Some(tokens[1].to_string())
                    } else {
                        None
                    };
                    in_macdef = true;
                    macdef_lines.clear();
                }
                _ => {}
            }
        }

        if let Some(entry) = current_entry.take() {
            if entry.machine == "default" {
                self.default_entry = Some(entry);
            } else {
                self.entries.push(entry);
            }
        }

        Ok(())
    }

    pub fn find(&self, host: &str) -> Option<&NetRcEntry> {
        for entry in &self.entries {
            if entry.machine == host {
                return Some(entry);
            }
        }
        if host.contains('.') {
            let domain_parts: Vec<&str> = host.split('.').collect();
            if domain_parts.len() >= 2 {
                let domain = domain_parts[domain_parts.len() - 2..].join(".");
                for entry in &self.entries {
                    if entry.machine == domain {
                        return Some(entry);
                    }
                }
            }
        }
        None
    }

    pub fn find_default(&self) -> Option<&NetRcEntry> {
        self.default_entry.as_ref()
    }

    pub fn get_credentials(&self, host: &str) -> Option<(String, String)> {
        if let Some(entry) = self.find(host)
            && let (Some(login), Some(pass)) = (&entry.login, &entry.password)
        {
            return Some((login.clone(), pass.clone()));
        }
        if let Some(def) = self.find_default()
            && let (Some(login), Some(pass)) = (&def.login, &def.password)
        {
            return Some((login.clone(), pass.clone()));
        }
        None
    }

    pub fn entries(&self) -> &[NetRcEntry] {
        &self.entries
    }
    pub fn default_entry(&self) -> Option<&NetRcEntry> {
        self.default_entry.as_ref()
    }
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
    pub fn len(&self) -> usize {
        self.entries.len() + if self.default_entry.is_some() { 1 } else { 0 }
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.default_entry.is_none()
    }

    pub fn to_map(&self) -> HashMap<String, (Option<String>, Option<String>)> {
        let mut map = HashMap::new();
        for entry in &self.entries {
            map.insert(
                entry.machine.clone(),
                (entry.login.clone(), entry.password.clone()),
            );
        }
        if let Some(def) = &self.default_entry {
            map.insert("default".into(), (def.login.clone(), def.password.clone()));
        }
        map
    }
}

impl Default for NetRcFile {
    fn default() -> Self {
        Self::new()
    }
}

pub fn find_netrc_file() -> Option<String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| {
            std::env::var_os("HOMEDRIVE").and_then(|d| {
                std::env::var_os("HOMEPATH").map(|p| {
                    let mut s = d.to_os_string();
                    s.push(p);
                    s
                })
            })
        });
    home.and_then(|h| {
        let h = h.to_string_lossy().to_string();
        for name in &[".netrc", "_netrc", ".netrc.txt"] {
            let candidate = format!("{}/{}", h, name);
            if Path::new(&candidate).exists() {
                return Some(candidate);
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_machine() {
        let content = "machine ftp.example.com\nlogin myuser\npassword mypass\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.len(), 1);
        let entry = &netrc.entries()[0];
        assert_eq!(entry.machine, "ftp.example.com");
        assert_eq!(entry.login.as_deref(), Some("myuser"));
        assert_eq!(entry.password.as_deref(), Some("mypass"));
    }

    #[test]
    fn test_parse_default_entry() {
        let content = "default\nlogin anonymous\npassword guest@\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.len(), 1);
        assert!(netrc.default_entry().is_some());
        let def = netrc.default_entry().unwrap();
        assert_eq!(def.machine, "default");
        assert_eq!(def.login.as_deref(), Some("anonymous"));
    }

    #[test]
    fn test_parse_multiple_machines() {
        let content = r#"machine ftp.example.com
login user1
pass pass1

machine ssh.example.com
login user2
password pass2
"#;
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.len(), 2);
        assert_eq!(netrc.entries()[0].machine, "ftp.example.com");
        assert_eq!(netrc.entries()[1].machine, "ssh.example.com");
    }

    #[test]
    fn test_find_exact_host() {
        let content = "machine ftp.example.com\nlogin user\npassword pass\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        let creds = netrc.get_credentials("ftp.example.com");
        assert!(creds.is_some());
        let (u, p) = creds.unwrap();
        assert_eq!(u, "user");
        assert_eq!(p, "pass");
    }

    #[test]
    fn test_find_unknown_host_returns_none() {
        let content = "machine ftp.example.com\nlogin user\npassword pass\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert!(netrc.get_credentials("unknown.host.com").is_none());
    }

    #[test]
    fn test_find_falls_back_to_default() {
        let content = r#"default
login anon
password guest@

machine special.example.com
login admin
password secret
"#;
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        let creds = netrc.get_credentials("other.example.com");
        assert!(creds.is_some());
        let (u, _p) = creds.unwrap();
        assert_eq!(u, "anon");
    }

    #[test]
    fn test_has_credentials() {
        let mut entry = NetRcEntry::new("host");
        entry.login = Some("user".into());
        entry.password = Some("pass".into());
        assert!(entry.has_credentials());

        let mut entry2 = NetRcEntry::new("host");
        entry2.login = Some("user".into());
        assert!(!entry2.has_credentials());
    }

    #[test]
    fn test_comments_and_blank_lines() {
        let content = r#"
# This is a comment
machine example.com
# Another comment
login user
   password pass
"#;
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.len(), 1);
        assert_eq!(netrc.entries()[0].login.as_deref(), Some("user"));
    }

    #[test]
    fn test_passwd_alias() {
        let content = "machine example.com\nlogin user\npasswd secret\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.entries()[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn test_account_field() {
        let content = "machine example.com\nlogin user\npassword pass\naccount acct123\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.entries()[0].account.as_deref(), Some("acct123"));
    }

    #[test]
    fn test_to_map() {
        let content = "machine host1\nlogin u1\npassword p1\ndefault\nlogin du\npassword dp\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        let map = netrc.to_map();
        assert!(map.contains_key("host1"));
        assert!(map.contains_key("default"));
    }

    #[test]
    fn test_empty_netrc() {
        let netrc = NetRcFile::new();
        assert!(netrc.is_empty());
        assert_eq!(netrc.len(), 0);
    }

    #[test]
    fn test_error_display() {
        let err = NetRcError::FileNotFound("/missing/.netrc".into());
        assert!(err.to_string().contains(".netrc"));

        let err2 = NetRcError::ParseError("bad token".into());
        assert!(err2.to_string().contains("parse error"));
    }

    #[test]
    fn test_case_insensitive_keywords() {
        let content = "MACHINE example.com\nLOGIN user\nPASSWORD pass\n";
        let mut netrc = NetRcFile::new();
        netrc.parse(content).unwrap();
        assert_eq!(netrc.entries()[0].login.as_deref(), Some("user"));
    }
}
