/// Unique identifier for a download group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

    /// Parse the high-order hexadecimal prefix used by aria2's `expandUnique`.
    /// Returns the normalized prefix and mask for unique matching.
    pub fn hex_prefix(hex_str: &str) -> Option<(u64, u64)> {
        let trimmed = hex_str.trim_start_matches("0x");
        if trimmed.is_empty() || trimmed.len() > 16 {
            return None;
        }
        let value = u64::from_str_radix(trimmed, 16).ok()?;
        let bits = trimmed.len() * 4;
        let mask = if bits == 64 {
            u64::MAX
        } else {
            u64::MAX << (64 - bits)
        };
        Some((value << (64 - bits), mask))
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
