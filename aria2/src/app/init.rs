//! Configuration initialization and path diagnostics.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::paths::{InitProfile, PathLayout, layout_for};

#[derive(Debug)]
pub struct InitRequest {
    pub profile: Option<InitProfile>,
    pub state_dir: Option<PathBuf>,
    pub download_dir: Option<PathBuf>,
    pub non_interactive: bool,
    pub force: bool,
}

pub fn initialize(request: InitRequest) -> Result<PathLayout, String> {
    if !request.non_interactive && !io::stdin().is_terminal() {
        return Err(
            "interactive initialization requires a terminal; use --profile and --non-interactive"
                .into(),
        );
    }
    let profile = match request.profile {
        Some(profile) => profile,
        None if request.non_interactive => InitProfile::System,
        None => choose_profile()?,
    };
    let state_dir = match (profile, request.state_dir) {
        (InitProfile::Custom, None) if !request.non_interactive => {
            Some(prompt_path("State/config directory", "aria2-state")?)
        }
        (InitProfile::Custom, None) => None,
        (_, state_dir) => state_dir,
    };
    let download_dir = match request.download_dir {
        Some(path) => Some(path),
        None if !request.non_interactive && profile == InitProfile::Custom => {
            Some(prompt_path("Download directory", "downloads")?)
        }
        None => None,
    };
    let layout = layout_for(profile, state_dir, download_dir)?;
    create_directory(&layout.config_dir)?;
    create_directory(&layout.state_dir)?;
    create_directory(&layout.cache_dir)?;
    create_directory(&layout.download_dir)?;

    let config = layout.config_file();
    if config.exists() {
        let backup = next_backup_path(&config);
        std::fs::copy(&config, &backup)
            .map_err(|e| format!("failed to back up {}: {e}", config.display()))?;
        eprintln!("Backed up existing configuration to {}", backup.display());
    }
    std::fs::write(&config, render_config(&layout))
        .map_err(|e| format!("failed to write {}: {e}", config.display()))?;
    Ok(layout)
}

pub fn print_paths(layout: &PathLayout) {
    println!("Config:        {}", layout.config_file().display());
    println!("State:         {}", layout.state_dir.display());
    println!("Cache:         {}", layout.cache_dir.display());
    println!("Download:      {}", layout.download_dir.display());
    println!(
        "Config exists: {}",
        if layout.config_file().exists() {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Writable:      {}",
        if is_writable(&layout.state_dir) {
            "yes"
        } else {
            "no"
        }
    );
}

fn choose_profile() -> Result<InitProfile, String> {
    println!("aria2-rust initialization\n");
    println!("[1] System user directories (recommended)");
    println!("[2] Current working directory");
    println!("[3] Executable directory");
    println!("[4] Portable mode");
    println!("[5] Custom directories");
    print!("Select [1-5]: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| e.to_string())?;
    match input.trim() {
        "1" | "" => Ok(InitProfile::System),
        "2" => Ok(InitProfile::Current),
        "3" => Ok(InitProfile::Executable),
        "4" => Ok(InitProfile::Portable),
        "5" => Ok(InitProfile::Custom),
        other => Err(format!("invalid profile selection: {other}")),
    }
}

fn prompt_path(label: &str, default_name: &str) -> Result<PathBuf, String> {
    let default_path = std::env::current_dir()
        .map_err(|e| format!("cannot determine current directory: {e}"))?
        .join(default_name);
    print!("{label} [{}]: ", default_path.display());
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|e| e.to_string())?;
    let value = input.trim();
    Ok(if value.is_empty() {
        default_path
    } else {
        PathBuf::from(value)
    })
}

fn create_directory(path: &Path) -> Result<(), String> {
    if path.exists() && !path.is_dir() {
        return Err(format!(
            "path exists but is not a directory: {}",
            path.display()
        ));
    }
    std::fs::create_dir_all(path).map_err(|e| format!("cannot create {}: {e}", path.display()))
}

fn is_writable(path: &Path) -> bool {
    let probe = path.join(format!(".aria2-write-test-{}", std::process::id()));
    match std::fs::write(&probe, []) {
        Ok(()) => std::fs::remove_file(probe).is_ok(),
        Err(_) => false,
    }
}

fn next_backup_path(path: &Path) -> PathBuf {
    let base = PathBuf::from(format!("{}.bak", path.display()));
    if !base.exists() {
        return base;
    }
    for index in 1.. {
        let candidate = PathBuf::from(format!("{}.bak.{index}", path.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn render_config(layout: &PathLayout) -> String {
    format!(
        "# aria2-rust configuration generated by --init\n# Paths are absolute so startup location does not change persistent state.\n\ndir={}\nsave-session={}\ninput-file={}\nlog={}\npid-file={}\ndht-file-path={}\ndht-file-path6={}\nserver-stat-of={}\nauto-save-interval=60\nsave-session-interval=60\nupdate-check=true\n",
        layout.download_dir.display(),
        layout.state_dir.join("session.txt").display(),
        layout.state_dir.join("session.txt").display(),
        layout.state_dir.join("aria2.log").display(),
        layout.state_dir.join("aria2.pid").display(),
        layout.state_dir.join("dht.dat").display(),
        layout.state_dir.join("dht6.dat").display(),
        layout.state_dir.join("server-stat.json").display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_absolute_config_and_backs_up_on_reset() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let downloads = root.path().join("downloads");
        let request = || InitRequest {
            profile: Some(InitProfile::Custom),
            state_dir: Some(state.clone()),
            download_dir: Some(downloads.clone()),
            non_interactive: true,
            force: false,
        };

        let layout = initialize(request()).unwrap();
        let config = std::fs::read_to_string(layout.config_file()).unwrap();
        assert!(config.contains(&format!("dir={}", downloads.display())));
        assert!(config.contains(&format!(
            "save-session={}",
            state.join("session.txt").display()
        )));

        initialize(request()).unwrap();
        assert!(layout.config_file().with_extension("conf.bak").exists());
    }
}
