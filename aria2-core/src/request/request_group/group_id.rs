/// Unique identifier for a download group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GroupId(pub u64);

impl GroupId {
    pub fn new(id: u64) -> Self {
        GroupId(id)
    }

    pub fn value(&self) -> u64 {
        self.0
    }

    /// Create GroupId from hex string (e.g., "deadbeef")
    ///
    /// Returns None if the string is not valid hex or too large for u64.
    pub fn from_hex_string(hex_str: &str) -> Option<Self> {
        let trimmed = hex_str.trim_start_matches("0x");
        if trimmed.is_empty() {
            return None;
        }
        let val = u64::from_str_radix(trimmed, 16).ok()?;
        Some(GroupId(val))
    }

    /// Generate a random GroupId using current timestamp + random
    pub fn new_random() -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut hasher = DefaultHasher::new();
        nanos.hash(&mut hasher);
        rand::random::<u64>().hash(&mut hasher);
        GroupId(hasher.finish())
    }

    /// Format GID as hex string (lowercase, no prefix)
    pub fn to_hex_string(&self) -> String {
        format!("{:016x}", self.0)
    }
}
