//! Cross-platform configuration, state, cache, and download path policy.

use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProfile {
    System,
    Current,
    Executable,
    Portable,
    Custom,
}

impl std::str::FromStr for InitProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "system" => Ok(Self::System),
            "current" => Ok(Self::Current),
            "executable" => Ok(Self::Executable),
            "portable" => Ok(Self::Portable),
            "custom" => Ok(Self::Custom),
            _ => Err(format!(
                "unknown profile '{value}'; expected system, current, executable, portable, or custom"
            )),
        }
    }
}

impl std::fmt::Display for InitProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::System => "system",
            Self::Current => "current",
            Self::Executable => "executable",
            Self::Portable => "portable",
            Self::Custom => "custom",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathLayout {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub download_dir: PathBuf,
}

impl PathLayout {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("aria2.conf")
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .or_else(|| {
            Some(
                Path::new(&env::var_os("HOMEDRIVE")?)
                    .join(env::var_os("HOMEPATH")?)
                    .into_os_string(),
            )
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn system_layout() -> PathLayout {
    let home = home_dir();
    if cfg!(windows) {
        let config_dir = env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join("aria2-rust");
        let state_dir = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("aria2-rust");
        PathLayout {
            config_dir,
            state_dir: state_dir.clone(),
            cache_dir: state_dir.join("cache"),
            download_dir: home.join("Downloads"),
        }
    } else if cfg!(target_os = "macos") {
        let base = home
            .join("Library")
            .join("Application Support")
            .join("aria2-rust");
        PathLayout {
            config_dir: base.clone(),
            state_dir: base.clone(),
            cache_dir: home.join("Library").join("Caches").join("aria2-rust"),
            download_dir: home.join("Downloads"),
        }
    } else {
        let config_base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let state_base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("state"));
        let cache_base = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"));
        PathLayout {
            config_dir: config_base.join("aria2-rust"),
            state_dir: state_base.join("aria2-rust"),
            cache_dir: cache_base.join("aria2-rust"),
            download_dir: home.join("Downloads"),
        }
    }
}

pub fn layout_for(
    profile: InitProfile,
    state_dir: Option<PathBuf>,
    download_dir: Option<PathBuf>,
) -> Result<PathLayout, String> {
    let current =
        env::current_dir().map_err(|e| format!("cannot determine current directory: {e}"))?;
    let executable = env::current_exe()
        .map_err(|e| format!("cannot determine executable directory: {e}"))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "executable has no parent directory".to_string())?;
    if profile == InitProfile::System {
        let mut layout = system_layout();
        if let Some(state_dir) = state_dir {
            layout.config_dir = state_dir.clone();
            layout.state_dir = state_dir.clone();
            layout.cache_dir = state_dir.join("cache");
        }
        if let Some(download_dir) = download_dir {
            layout.download_dir = download_dir;
        }
        return Ok(layout);
    }
    let base = match profile {
        InitProfile::System => unreachable!("system profile is handled above"),
        InitProfile::Current => current.clone(),
        InitProfile::Executable => executable,
        InitProfile::Portable => executable,
        InitProfile::Custom => state_dir
            .clone()
            .ok_or_else(|| "--state-dir is required for the custom profile".to_string())?,
    };
    let default_state = if profile == InitProfile::Custom {
        base.clone()
    } else {
        base.join(".aria2")
    };
    let state = state_dir.unwrap_or(default_state);
    let download = download_dir.unwrap_or_else(|| {
        if profile == InitProfile::Portable {
            base.join("downloads")
        } else if profile == InitProfile::Current {
            current.join("downloads")
        } else {
            system_layout().download_dir
        }
    });
    Ok(PathLayout {
        config_dir: state.clone(),
        state_dir: state,
        cache_dir: base.join(".aria2").join("cache"),
        download_dir: download,
    })
}

pub fn default_config_candidates() -> Vec<PathBuf> {
    let home = home_dir();
    let mut candidates = vec![home.join(".aria2").join("aria2.conf")];
    let standard = system_layout().config_file();
    if !candidates.contains(&standard) {
        candidates.push(standard);
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_profile_requires_state_dir() {
        let result = layout_for(InitProfile::Custom, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn profile_names_are_case_insensitive() {
        assert_eq!(
            "PORTABLE".parse::<InitProfile>().unwrap(),
            InitProfile::Portable
        );
    }

    #[test]
    fn explicit_directories_override_every_profile() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state with spaces");
        let downloads = root.path().join("downloads");
        let layout = layout_for(
            InitProfile::System,
            Some(state.clone()),
            Some(downloads.clone()),
        )
        .unwrap();
        assert_eq!(layout.config_dir, state);
        assert_eq!(layout.state_dir, layout.config_dir);
        assert_eq!(layout.download_dir, downloads);
    }

    #[test]
    fn portable_profile_is_relative_to_executable_directory() {
        let layout = layout_for(InitProfile::Portable, None, None).unwrap();
        assert!(layout.config_dir.ends_with(std::path::Path::new(".aria2")));
        assert!(
            layout
                .download_dir
                .ends_with(std::path::Path::new("downloads"))
        );
    }
}
