//! Path normalization and joining.
//!
//! - `normalize_path()`: resolve `.` / `..` and collapse duplicate `/`
//! - `join_path()`: combine base + relative path with normalization
//!
//! The normalization state machine mirrors the C++ `uri::normalizePath()` exactly.

// ---------------------------------------------------------------------------
// normalizePath — state-machine path normalizer (mirrors C++ exactly)
// ---------------------------------------------------------------------------

/// States for the path-normalization state machine.
///
/// Mirrors the anonymous enum in C++ `uri.cc`:
/// `NPATH_START, NPATH_SLASH, NPATH_SDOT, NPATH_DDOT, NPATH_PATHCOMP`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathState {
    Start,
    Slash,
    SingleDot,
    DoubleDot,
    PathComp,
}

/// Normalize a path by:
/// 1. Removing successive `/` (duplicate slashes).
/// 2. Resolving `.` (current directory) components.
/// 3. Resolving `..` (parent directory) components — excess `..` are discarded.
///
/// The resulting path starts with `/` only if the input starts with `/`.
///
/// Mirrors C++ `uri::normalizePath()` exactly, including the state machine
/// and range-based compaction algorithm.
pub fn normalize_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return String::new();
    }

    let mut state = PathState::Start;
    let mut start_with_slash = false;
    // `range` stores pairs (start, end) of path segments to keep.
    // In C++ this is `std::vector<int>` used in pairs.
    let mut range: Vec<usize> = Vec::with_capacity(32);

    for (i, &b) in bytes.iter().enumerate() {
        let ch = b as char;
        state = match state {
            PathState::Start => match ch {
                '.' => {
                    range.push(i);
                    PathState::SingleDot
                }
                '/' => {
                    start_with_slash = true;
                    PathState::Slash
                }
                _ => {
                    range.push(i);
                    PathState::PathComp
                }
            },
            PathState::Slash => match ch {
                '.' => {
                    range.push(i);
                    PathState::SingleDot
                }
                '/' => {
                    // Drop duplicate '/'.
                    PathState::Slash
                }
                _ => {
                    range.push(i);
                    PathState::PathComp
                }
            },
            PathState::SingleDot => match ch {
                '.' => PathState::DoubleDot,
                '/' => {
                    // Drop path component '.'.
                    range.pop();
                    PathState::Slash
                }
                _ => PathState::PathComp,
            },
            PathState::DoubleDot => match ch {
                '/' => {
                    // Drop previous path component before '..'.
                    for _ in 0..3 {
                        range.pop();
                    }
                    PathState::Slash
                }
                _ => PathState::PathComp,
            },
            PathState::PathComp => {
                if ch == '/' {
                    // Record start of next segment (position after '/').
                    range.push(i + 1);
                    PathState::Slash
                } else {
                    PathState::PathComp
                }
            }
        };
    }

    // Handle end-of-string transitions.
    match state {
        PathState::SingleDot => {
            range.pop();
        }
        PathState::DoubleDot => {
            for _ in 0..3 {
                range.pop();
            }
        }
        PathState::PathComp => {
            range.push(len);
        }
        _ => {}
    }

    // Reconstruct the string from the kept ranges.
    let mut out = Vec::with_capacity(len);
    if start_with_slash {
        out.push(b'/');
    }

    let mut i = 0;
    while i + 1 < range.len() {
        let a = range[i];
        let b = range[i + 1];
        out.extend_from_slice(&bytes[a..b]);
        i += 2;
    }

    String::from_utf8(out).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// joinPath — combine base path with relative path
// ---------------------------------------------------------------------------

/// Join a base path with a new (possibly relative) path, then normalize.
///
/// If `new_path` starts with `/`, it is treated as absolute and `base_path`
/// is ignored (after normalization). Otherwise, `new_path` is appended to
/// `base_path` (with a `/` separator if needed) before normalization.
///
/// Mirrors C++ `uri::joinPath()`.
pub fn join_path(base_path: &str, new_path: &str) -> String {
    join_path_inner(base_path, new_path)
}

fn join_path_inner(base_path: &str, new_path: &str) -> String {
    if new_path.is_empty() {
        return base_path.to_owned();
    }

    // If new_path is absolute or base_path is empty, just normalize new_path.
    if base_path.is_empty() || new_path.starts_with('/') {
        return normalize_path(new_path);
    }

    // Append new_path to base_path.
    let combined = if base_path.ends_with('/') {
        format!("{}{}", base_path, new_path)
    } else {
        format!("{}/{}", base_path, new_path)
    };

    normalize_path(&combined)
}
