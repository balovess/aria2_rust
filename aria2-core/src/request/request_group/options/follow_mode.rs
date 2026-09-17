/// How a metadata file should be handled after it is downloaded.
///
/// `None` on [`DownloadOptions`] means that the option was not explicitly
/// supplied and the aria2 default applies.  When supplied, the wire values
/// are `true`, `false`, and `mem`; keeping `mem` as a distinct enum variant is
/// necessary because it changes the disk-writer lifecycle, not just handler
/// selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowMode {
    /// Follow the downloaded metadata and keep the source file on disk.
    Follow,
    /// Do not follow the downloaded metadata.
    Disabled,
    /// Follow metadata from an in-memory buffer without creating a source file.
    Memory,
}

impl FollowMode {
    /// Convert the boolean form accepted by legacy RPC callers.
    pub const fn from_bool(value: bool) -> Self {
        if value { Self::Follow } else { Self::Disabled }
    }

    /// Parse an aria2 option value. Invalid values are rejected so callers
    /// can preserve the configured default instead of silently changing mode.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(Self::Follow),
            "false" | "0" => Some(Self::Disabled),
            "mem" => Some(Self::Memory),
            _ => None,
        }
    }

    /// Return the canonical aria2 wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "true",
            Self::Disabled => "false",
            Self::Memory => "mem",
        }
    }

    /// Whether the metadata post-download handler should be installed.
    pub const fn follows(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether the source should be downloaded into memory.
    pub const fn is_memory(self) -> bool {
        matches!(self, Self::Memory)
    }
}
