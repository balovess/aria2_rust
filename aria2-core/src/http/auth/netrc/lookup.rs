//! Locate the default `.netrc` file on the system.

use std::path::Path;

/// Locate the user's `.netrc` file by checking standard locations.
///
/// On Unix: `$HOME/.netrc`
/// On Windows: `%USERPROFILE%\_netrc` or `%HOMEDRIVE%%HOMEPATH%\_netrc`
///
/// Also checks `.netrc.txt` as some tools use that extension.
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
