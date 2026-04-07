use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionType {
    String,
    Integer,
    Float,
    Boolean,
    List,
    Enum,
    Path,
    Size,
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String => write!(f, "string"),
            Self::Integer => write!(f, "integer"),
            Self::Float => write!(f, "float"),
            Self::Boolean => write!(f, "boolean"),
            Self::List => write!(f, "list"),
            Self::Enum => write!(f, "enum"),
            Self::Path => write!(f, "path"),
            Self::Size => write!(f, "size"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionCategory {
    General,
    HttpFtp,
    BitTorrent,
    Rpc,
    Advanced,
}

impl fmt::Display for OptionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::General => write!(f, "general"),
            Self::HttpFtp => write!(f, "http/ftp"),
            Self::BitTorrent => write!(f, "bittorrent"),
            Self::Rpc => write!(f, "rpc"),
            Self::Advanced => write!(f, "advanced"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
    None,
}

impl Default for OptionValue {
    fn default() -> Self { Self::None }
}

impl fmt::Display for OptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Str(s) => write!(f, "{}", s),
            Self::Int(n) => write!(f, "{}", n),
            Self::Float(v) => write!(f, "{}", v),
            Self::Bool(b) => write!(f, "{}", b),
            Self::List(items) => write!(f, "{}", items.join(",")),
            Self::None => write!(f, ""),
        }
    }
}

impl From<&OptionValue> for serde_json::Value {
    fn from(v: &OptionValue) -> Self {
        match v {
            OptionValue::Str(s) => serde_json::json!(s),
            OptionValue::Int(n) => serde_json::json!(*n),
            OptionValue::Float(v) => serde_json::json!(*v),
            OptionValue::Bool(b) => serde_json::json!(*b),
            OptionValue::List(items) => serde_json::json!(items),
            OptionValue::None => serde_json::Value::Null,
        }
    }
}

impl From<serde_json::Value> for OptionValue {
    fn from(val: serde_json::Value) -> Self {
        match val {
            serde_json::Value::String(s) => Self::Str(s),
            serde_json::Value::Number(n) if n.is_i64() => Self::Int(n.as_i64().unwrap()),
            serde_json::Value::Number(n) if n.is_f64() => Self::Float(n.as_f64().unwrap()),
            serde_json::Value::Bool(b) => Self::Bool(b),
            serde_json::Value::Array(arr) => Self::List(
                arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            ),
            _ => Self::None,
        }
    }
}

impl OptionValue {
    pub fn as_str(&self) -> Option<&str> { if let Self::Str(s) = self { Some(s) } else { None } }
    pub fn as_i64(&self) -> Option<i64> { if let Self::Int(n) = self { Some(*n) } else { None } }
    pub fn as_f64(&self) -> Option<f64> { if let Self::Float(v) = self { Some(*v) } else { None } }
    pub fn as_bool(&self) -> Option<bool> { if let Self::Bool(b) = self { Some(*b) } else { None } }
    pub fn as_list(&self) -> Option<&Vec<String>> { if let Self::List(l) = self { Some(l) } else { None } }
    pub fn is_none(&self) -> bool { matches!(self, Self::None) }

    pub fn parse_size_str(s: &str) -> u64 {
        let s = s.trim();
        let (num_part, suffix) = if s.len() > 1 {
            let last_char = s.chars().last().unwrap();
            match last_char {
                'K' | 'k' => (&s[..s.len()-1], 1024u64),
                'M' | 'm' => (&s[..s.len()-1], 1024*1024),
                'G' | 'g' => (&s[..s.len()-1], 1024u64*1024*1024),
                'T' | 't' => (&s[..s.len()-1], 1024u64*1024*1024*1024),
                _ => (s, 1u64),
            }
        } else {
            (s, 1u64)
        };
        num_part.parse::<f64>().map(|n| (n * suffix as f64) as u64).unwrap_or(0)
    }

    pub fn to_size_string(bytes: u64) -> String {
        const K: u64 = 1024;
        const M: u64 = K * K;
        const G: u64 = M * K;
        const T: u64 = G * K;
        if bytes >= T { format!("{}T", bytes as f64 / T as f64) }
        else if bytes >= G { format!("{}G", bytes as f64 / G as f64) }
        else if bytes >= M { format!("{}M", bytes as f64 / M as f64) }
        else if bytes >= K { format!("{}K", bytes as f64 / K as f64) }
        else { format!("{}", bytes) }
    }
}

#[derive(Debug, Clone)]
pub struct OptionDef {
    name: String,
    short_name: Option<char>,
    opt_type: OptionType,
    default_value: OptionValue,
    description: String,
    category: OptionCategory,
    min: Option<i64>,
    max: Option<u64>,
    deprecated: bool,
    hidden: bool,
}

impl OptionDef {
    pub fn new(name: impl Into<String>, opt_type: OptionType) -> Self {
        Self {
            name: name.into(),
            short_name: None,
            opt_type,
            default_value: OptionValue::None,
            description: String::new(),
            category: OptionCategory::General,
            min: None,
            max: None,
            deprecated: false,
            hidden: false,
        }
    }

    pub fn short(mut self, c: char) -> Self { self.short_name = Some(c); self }
    pub fn default(mut self, v: impl Into<OptionValue>) -> Self { self.default_value = v.into(); self }
    pub fn desc(mut self, d: impl Into<String>) -> Self { self.description = d.into(); self }
    pub fn category(mut self, c: OptionCategory) -> Self { self.category = c; self }
    pub fn range(mut self, min: i64, max: u64) -> Self { self.min = Some(min); self.max = Some(max); self }
    pub fn deprecated(mut self) -> Self { self.deprecated = true; self }
    pub fn hidden(mut self) -> Self { self.hidden = true; self }

    pub fn name(&self) -> &str { &self.name }
    pub fn short_name(&self) -> Option<char> { self.short_name }
    pub fn opt_type(&self) -> OptionType { self.opt_type }
    pub fn default_value(&self) -> &OptionValue { &self.default_value }
    pub fn get_category(&self) -> OptionCategory { self.category }
    pub fn is_deprecated(&self) -> bool { self.deprecated }
    pub fn is_hidden(&self) -> bool { self.hidden }

    pub fn parse_value(&self, s: &str) -> Result<OptionValue, String> {
        if s.is_empty() { return Ok(self.default_value.clone()); }
        match self.opt_type {
            OptionType::String | OptionType::Path | OptionType::Enum => Ok(OptionValue::Str(s.to_string())),
            OptionType::Integer => s.parse::<i64>()
                .map(|n| {
                    if let Some(min) = self.min { if n < min { return Err(format!("value {} < minimum {}", n, min)); } }
                    if let Some(max) = self.max { if n < 0 || n as u64 > max { return Err(format!("value {} exceeds maximum {}", n, max)); } }
                    Ok(OptionValue::Int(n))
                })
                .map_err(|e| format!("invalid integer '{}': {}", s, e))?,
            OptionType::Size => Ok(OptionValue::Int(OptionValue::parse_size_str(s) as i64)),
            OptionType::Float => s.parse::<f64>().map(OptionValue::Float).map_err(|e| format!("invalid float '{}': {}", s, e)),
            OptionType::Boolean => match s.to_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Ok(OptionValue::Bool(true)),
                "false" | "no" | "0" | "off" => Ok(OptionValue::Bool(false)),
                _ => Err(format!("invalid boolean '{}'", s)),
            },
            OptionType::List => Ok(OptionValue::List(s.split(',').map(|x| x.trim().to_string()).collect())),
        }
    }
}

#[derive(Clone)]
pub struct OptionRegistry {
    options: HashMap<String, OptionDef>,
}

impl OptionRegistry {
    pub fn new() -> Self {
        let mut reg = Self { options: HashMap::new() };
        reg.register_core_options();
        reg
    }

    pub fn register(&mut self, def: OptionDef) {
        self.options.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&OptionDef> { self.options.get(name) }
    pub fn contains(&self, name: &str) -> bool { self.options.contains_key(name) }
    pub fn all(&self) -> &HashMap<String, OptionDef> { &self.options }
    pub fn count(&self) -> usize { self.options.len() }
    pub fn by_category(&self, cat: OptionCategory) -> Vec<&OptionDef> {
        self.options.values().filter(|d| d.category == cat).collect()
    }

    fn register_core_options(&mut self) {
        self.register(OptionDef::new("dir", OptionType::Path)
            .short('d').default(OptionValue::Str(".".into())).desc("Save directory").category(OptionCategory::General));
        self.register(OptionDef::new("out", OptionType::String)
            .short('o').desc("Output filename").category(OptionCategory::General));
        self.register(OptionDef::new("log", OptionType::Path)
            .default(OptionValue::Str("-".into())).desc("Log file path").category(OptionCategory::General));
        self.register(OptionDef::new("log-level", OptionType::Enum)
            .default(OptionValue::Str("info".into())).desc("Log level (debug/info/notice/warn/error)").category(OptionCategory::General));
        self.register(OptionDef::new("console-log-level", OptionType::Enum)
            .default(OptionValue::Str("notice".into())).desc("Console log level").category(OptionCategory::General));
        self.register(OptionDef::new("summary-interval", OptionType::Integer)
            .default(OptionValue::Int(60)).range(0, 3600).desc("Progress summary interval in seconds").category(OptionCategory::General));
        self.register(OptionDef::new("conf-path", OptionType::Path)
            .desc("Configuration file path").category(OptionCategory::General));
        self.register(OptionDef::new("input-file", OptionType::Path)
            .short('i').desc("URI input file").category(OptionCategory::General));
        self.register(OptionDef::new("save-session", OptionType::Path)
            .desc("Session save file").category(OptionCategory::General));
        self.register(OptionDef::new("save-session-interval", OptionType::Integer)
            .default(OptionValue::Int(0)).desc("Auto-save session interval (0=disabled)").category(OptionCategory::General));
        self.register(OptionDef::new("auto-save-interval", OptionType::Integer)
            .default(OptionValue::Int(60)).range(0, 600).desc("Auto-save interval").category(OptionCategory::General));
        self.register(OptionDef::new("enable-color", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Enable colored output").category(OptionCategory::General));
        self.register(OptionDef::new("quiet", OptionType::Boolean)
            .short('q').default(OptionValue::Bool(false)).desc("Quiet mode").category(OptionCategory::General));
        self.register(OptionDef::new("dry-run", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Dry run (check only, no download)").category(OptionCategory::General));

        self.register(OptionDef::new("all-proxy", OptionType::String)
            .desc("Global proxy URL").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("http-proxy", OptionType::String)
            .desc("HTTP proxy URL").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("https-proxy", OptionType::String)
            .desc("HTTPS proxy URL").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("ftp-proxy", OptionType::String)
            .desc("FTP proxy URL").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("no-proxy", OptionType::List)
            .desc("Proxy exclusion list (comma-separated domains)").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("user-agent", OptionType::String)
            .default(OptionValue::Str("aria2/1.37.0-Rust".into())).desc("User-Agent header").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("referer", OptionType::String)
            .desc("Referer header").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("header", OptionType::List)
            .desc("Custom headers (Header:Value pairs)").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("load-cookies", OptionType::Path)
            .desc("Cookie file to load").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("save-cookies", OptionType::Path)
            .desc("Cookie file to save").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("connect-timeout", OptionType::Integer)
            .default(OptionValue::Int(60)).range(1, 600).desc("Connect timeout in seconds").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("timeout", OptionType::Integer)
            .default(OptionValue::Int(60)).range(1, 600).desc("I/O timeout in seconds").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("max-tries", OptionType::Integer)
            .default(OptionValue::Int(5)).range(0, 100).desc("Max retry attempts").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("retry-wait", OptionType::Integer)
            .default(OptionValue::Int(0)).range(0, 3600).desc("Retry wait time in seconds").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("split", OptionType::Integer)
            .short('s').default(OptionValue::Int(5)).range(1, 16).desc("Connections per download").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("min-split-size", OptionType::Size)
            .default(OptionValue::Int((20 * 1024 * 1024) as i64)).desc("Min split size").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("max-connection-per-server", OptionType::Integer)
            .default(OptionValue::Int(1)).range(1, 16).desc("Max connections per server").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("check-certificate", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Verify SSL certificate").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("ca-certificate", OptionType::Path)
            .desc("CA certificate file").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("allow-overwrite", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Allow overwriting existing files").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("auto-file-renaming", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Auto rename conflicting files").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("continue", OptionType::Boolean)
            .short('c').default(OptionValue::Bool(true)).desc("Resume partial downloads").category(OptionCategory::HttpFtp));
        self.register(OptionDef::new("remote-time", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Use remote file timestamp").category(OptionCategory::HttpFtp));

        self.register(OptionDef::new("seed-time", OptionType::Float)
            .default(OptionValue::Float(0.0)).desc("Seeding time in minutes (0=infinite)").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("seed-ratio", OptionType::Float)
            .default(OptionValue::Float(1.0)).desc("Share ratio threshold").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-max-peers", OptionType::Integer)
            .default(OptionValue::Int(55)).range(0, 512).desc("Max peers per torrent").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-request-peer-speed-limit", OptionType::Size)
            .default(OptionValue::Int((50 * 1024) as i64)).desc("Min peer speed to stay connected").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-max-open-files", OptionType::Integer)
            .default(OptionValue::Int(100)).range(10, 4096).desc("Max open files for BT").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-seed-unverified", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Seed without verifying hash").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-save-metadata", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Save metadata as .torrent file").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-force-encryption", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Force BT encryption").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-min-crypto-level", OptionType::Enum)
            .default(OptionValue::Str("plain".into())).desc("Min crypto level (plain/arc4)").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("bt-enable-lpd", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Enable Local Peer Discovery").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("enable-dht", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Enable DHT").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("dht-listen-port", OptionType::Integer)
            .default(OptionValue::Int(6881)).range(1024, 65535).desc("DHT listen port").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("dht-message-path", OptionType::Path)
            .desc("DHT message cache path").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("enable-peer-exchange", OptionType::Boolean)
            .default(OptionValue::Bool(true)).desc("Enable PEX").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("follow-torrent", OptionType::Enum)
            .default(OptionValue::Str("true".into())).desc("Auto-handle .torrent (true/false/mem)").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("on-bt-download-complete", OptionType::String)
            .desc("Command on BT download complete").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("on-bt-download-error", OptionType::String)
            .desc("Command on BT download error").category(OptionCategory::BitTorrent));
        self.register(OptionDef::new("listen-port", OptionType::String)
            .default(OptionValue::Str("6881-6999".into())).desc("Listening port range").category(OptionCategory::BitTorrent));

        self.register(OptionDef::new("enable-rpc", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Enable JSON-RPC/XML-RPC server").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-listen-all", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Listen on all network interfaces").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-listen-port", OptionType::Integer)
            .default(OptionValue::Int(6800)).range(1024, 65535).desc("RPC server port").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-secret", OptionType::String)
            .desc("RPC secret token for authorization").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-user", OptionType::String)
            .desc("RPC Basic Auth username").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-passwd", OptionType::String)
            .desc("RPC Basic Auth password").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-allow-origin", OptionType::String)
            .desc("CORS Allow-Origin value").category(OptionCategory::Rpc));
        self.register(OptionDef::new("rpc-listen-address", OptionType::String)
            .default(OptionValue::Str("127.0.0.1".into())).desc("RPC server bind address").category(OptionCategory::Rpc));

        self.register(OptionDef::new("file-allocation", OptionType::Enum)
            .default(OptionValue::Str("prealloc".into())).desc("File allocation method (none/prealloc/falloc/trunc)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("max-concurrent-downloads", OptionType::Integer)
            .default(OptionValue::Int(5)).range(1, 256).desc("Max concurrent downloads").category(OptionCategory::Advanced));
        self.register(OptionDef::new("max-overall-download-limit", OptionType::Size)
            .default(OptionValue::Int(0)).desc("Overall download speed limit (0=unlimited)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("max-download-limit", OptionType::Size)
            .default(OptionValue::Int(0)).desc("Per-task download limit (0=unlimited)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("max-overall-upload-limit", OptionType::Size)
            .default(OptionValue::Int(0)).desc("Overall upload speed limit (0=unlimited)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("max-upload-limit", OptionType::Size)
            .default(OptionValue::Int(0)).desc("Per-task upload limit (0=unlimited)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("piece-length", OptionType::Size)
            .default(OptionValue::Int((1024 * 1024) as i64)).desc("BT piece length").category(OptionCategory::Advanced));
        self.register(OptionDef::new("disk-cache", OptionType::Size)
            .default(OptionValue::Int(0)).desc("Disk cache size (0=disabled)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("stop", OptionType::Integer)
            .default(OptionValue::Int(0)).range(0, 86400).desc("Stop after N seconds of completion (0=never)").category(OptionCategory::Advanced));
        self.register(OptionDef::new("force-save", OptionType::Boolean)
            .default(OptionValue::Bool(false)).desc("Force save state on every change").category(OptionCategory::Advanced));
    }
}

impl Default for OptionRegistry {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_option_type_display() {
        assert_eq!(OptionType::String.to_string(), "string");
        assert_eq!(OptionType::Boolean.to_string(), "boolean");
        assert_eq!(OptionType::Size.to_string(), "size");
    }

    #[test]
    fn test_option_category_display() {
        assert_eq!(OptionCategory::General.to_string(), "general");
        assert_eq!(OptionCategory::BitTorrent.to_string(), "bittorrent");
    }

    #[test]
    fn test_option_value_variants() {
        let s = OptionValue::Str("hello".into());
        assert_eq!(s.as_str().unwrap(), "hello");

        let n = OptionValue::Int(42);
        assert_eq!(n.as_i64().unwrap(), 42);

        let b = OptionValue::Bool(true);
        assert!(b.as_bool().unwrap());

        let l = OptionValue::List(vec!["a".into(), "b".into()]);
        assert_eq!(l.as_list().unwrap().len(), 2);

        let none = OptionValue::None;
        assert!(none.is_none());
    }

    #[test]
    fn test_option_value_display() {
        assert_eq!(OptionValue::Str("test".into()).to_string(), "test");
        assert_eq!(OptionValue::Int(99).to_string(), "99");
        assert_eq!(OptionValue::Bool(true).to_string(), "true");
        assert_eq!(OptionValue::List(vec!["x".into(), "y".into()]).to_string(), "x,y");
    }

    #[test]
    fn test_option_value_to_json() {
        let v = OptionValue::Str("hello".into());
        let jv: serde_json::Value = (&v).into();
        assert_eq!(jv, "hello");

        let v2 = OptionValue::Int(123);
        let jv2: serde_json::Value = (&v2).into();
        assert_eq!(jv2, 123);

        let v3 = OptionValue::Bool(false);
        let jv3: serde_json::Value = (&v3).into();
        assert_eq!(jv3, false);

        let v4 = OptionValue::List(vec!["a".into()]);
        let jv4: serde_json::Value = (&v4).into();
        assert!(jv4.is_array());
    }

    #[test]
    fn test_option_value_from_json() {
        let ov: OptionValue = serde_json::json!("test string").into();
        assert_eq!(ov.as_str().unwrap(), "test string");

        let ov2: OptionValue = serde_json::json!(42).into();
        assert_eq!(ov2.as_i64().unwrap(), 42);

        let ov3: OptionValue = serde_json::json!(true).into();
        assert!(ov3.as_bool().unwrap());

        let ov4: OptionValue = serde_json::json!(["a", "b"]).into();
        assert_eq!(ov4.as_list().unwrap().len(), 2);
    }

    #[test]
    fn test_size_parsing() {
        assert_eq!(OptionValue::parse_size_str("100"), 100);
        assert_eq!(OptionValue::parse_size_str("1K"), 1024);
        assert_eq!(OptionValue::parse_size_str("2M"), 2 * 1024 * 1024);
        assert_eq!(OptionValue::parse_size_str("1G"), 1024u64 * 1024 * 1024);
        assert_eq!(OptionValue::parse_size_str("0"), 0);
    }

    #[test]
    fn test_size_display() {
        assert!(OptionValue::to_size_string(500).contains("500"));
        assert!(OptionValue::to_size_string(2048).contains("K"));
        assert!(OptionValue::to_size_string(3 * 1024 * 1024).contains("M"));
    }

    #[test]
    fn test_option_def_builder() {
        let def = OptionDef::new("split", OptionType::Integer)
            .short('s')
            .default(OptionValue::Int(5))
            .desc("Connections per download")
            .range(1, 16)
            .category(OptionCategory::HttpFtp);
        assert_eq!(def.name(), "split");
        assert_eq!(def.short_name(), Some('s'));
        assert_eq!(def.opt_type(), OptionType::Integer);
        assert!(!def.is_deprecated());
        assert!(!def.is_hidden());
    }

    #[test]
    fn test_option_def_parse_integer() {
        let def = OptionDef::new("split", OptionType::Integer).range(1, 16);
        let v = def.parse_value("5").unwrap();
        assert_eq!(v.as_i64().unwrap(), 5);

        let err = def.parse_value("0");
        assert!(err.is_err());

        let err2 = def.parse_value("abc");
        assert!(err2.is_err());
    }

    #[test]
    fn test_option_def_parse_boolean() {
        let def = OptionDef::new("verbose", OptionType::Boolean);
        assert!(def.parse_value("true").unwrap().as_bool().unwrap());
        assert!(def.parse_value("yes").unwrap().as_bool().unwrap());
        assert!(def.parse_value("1").unwrap().as_bool().unwrap());
        assert!(!def.parse_value("false").unwrap().as_bool().unwrap());
        assert!(!def.parse_value("no").unwrap().as_bool().unwrap());
        assert!(def.parse_value("invalid").is_err());
    }

    #[test]
    fn test_option_def_parse_list() {
        let def = OptionDef::new("header", OptionType::List);
        let v = def.parse_value("X-Custom:foo,X-Bar:baz").unwrap();
        assert_eq!(v.as_list().unwrap().len(), 2);
    }

    #[test]
    fn test_option_def_parse_empty_uses_default() {
        let def = OptionDef::new("dir", OptionType::Path).default(OptionValue::Str("/tmp".into()));
        let v = def.parse_value("").unwrap();
        assert_eq!(v.as_str().unwrap(), "/tmp");
    }

    #[test]
    fn test_registry_creation() {
        let reg = OptionRegistry::new();
        assert!(reg.count() >= 60);
        assert!(reg.get("split").is_some());
        assert!(reg.get("nonexistent-option").is_none());
    }

    #[test]
    fn test_registry_by_category() {
        let reg = OptionRegistry::new();
        let general = reg.by_category(OptionCategory::General);
        let bt = reg.by_category(OptionCategory::BitTorrent);
        let rpc = reg.by_category(OptionCategory::Rpc);
        assert!(!general.is_empty());
        assert!(!bt.is_empty());
        assert!(!rpc.is_empty());
    }

    #[test]
    fn test_registry_defaults_are_valid() {
        let reg = OptionRegistry::new();
        for def in reg.all().values() {
            if !matches!(def.default_value, OptionValue::None) {
                let parsed = def.parse_value(&def.default_value.to_string());
                assert!(parsed.is_ok(), "Default value for '{}' failed to re-parse: {:?}", def.name, parsed.err());
            }
        }
    }

    #[test]
    fn test_default_registry() {
        let reg = OptionRegistry::default();
        assert!(reg.count() > 0);
    }
}
