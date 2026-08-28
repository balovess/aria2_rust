//! Explicit, backup-first maintenance for user configuration files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use aria2_core::config::ConfigParser;

pub(super) const RESET_CONFIG_TEMPLATE: &str = "# aria2-rust configuration\n# This file was reset to built-in defaults. Uncomment and adjust options as needed.\n\n# dir=downloads\n# enable-rpc=true\n# rpc-secret=change-this-secret\n";

pub(super) struct ConfigFileUpdate {
    pub backup_path: PathBuf,
    pub changed_lines: usize,
}

pub(super) fn repair(path: &Path) -> Result<ConfigFileUpdate, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read config file '{}': {error}", path.display()))?;
    let mut parser = ConfigParser::new();
    parser.parse_file(
        path.to_str()
            .ok_or_else(|| format!("config file path '{}' is not valid UTF-8", path.display()))?,
    );

    let invalid_lines = parser
        .errors()
        .iter()
        .enumerate()
        .filter_map(|(index, _)| parser.error_context(index).map(|context| context.line))
        .collect::<BTreeSet<_>>();
    if invalid_lines.is_empty() {
        return Err("no invalid configuration entries were found".to_string());
    }

    let repaired = rewrite_invalid_lines(&content, &invalid_lines);
    replace_with_backup(path, &repaired, invalid_lines.len())
}

pub(super) fn reset(path: &Path) -> Result<ConfigFileUpdate, String> {
    replace_with_backup(path, RESET_CONFIG_TEMPLATE, 0)
}

fn rewrite_invalid_lines(content: &str, invalid_lines: &BTreeSet<usize>) -> String {
    content
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            if invalid_lines.contains(&(index + 1)) {
                format!("# aria2-rust disabled invalid config: {line}")
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn replace_with_backup(
    path: &Path,
    replacement: &str,
    changed_lines: usize,
) -> Result<ConfigFileUpdate, String> {
    if !path.is_file() {
        return Err(format!("config file not found: {}", path.display()));
    }

    let backup_path = next_backup_path(path);
    std::fs::copy(path, &backup_path).map_err(|error| {
        format!(
            "failed to create config backup '{}' from '{}': {error}",
            backup_path.display(),
            path.display()
        )
    })?;
    std::fs::write(path, replacement).map_err(|error| {
        format!(
            "failed to write repaired config '{}'; backup remains at '{}': {error}",
            path.display(),
            backup_path.display()
        )
    })?;

    Ok(ConfigFileUpdate {
        backup_path,
        changed_lines,
    })
}

fn next_backup_path(path: &Path) -> PathBuf {
    let base = PathBuf::from(format!("{}.bak", path.display()));
    if !base.exists() {
        return base;
    }

    for suffix in 1.. {
        let candidate = PathBuf::from(format!("{}.bak.{suffix}", path.display()));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded backup suffix search must return")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_comments_invalid_lines_and_preserves_a_backup() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("aria2.conf");
        std::fs::write(
            &path,
            "continue=true\noptimize-concurrent-downloads=true\nsplit=zero\n",
        )
        .expect("write config");

        let update = repair(&path).expect("repair config");
        let repaired = std::fs::read_to_string(&path).expect("read repaired config");

        assert_eq!(update.changed_lines, 1);
        assert_eq!(
            std::fs::read_to_string(&update.backup_path).expect("read backup"),
            "continue=true\noptimize-concurrent-downloads=true\nsplit=zero\n"
        );
        assert!(repaired.contains("continue=true"));
        assert!(repaired.contains("optimize-concurrent-downloads=true"));
        assert!(repaired.contains("# aria2-rust disabled invalid config: split=zero"));
    }

    #[test]
    fn reset_replaces_the_file_after_creating_a_backup() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("aria2.conf");
        std::fs::write(&path, "split=zero\n").expect("write config");

        let update = reset(&path).expect("reset config");

        assert_eq!(update.changed_lines, 0);
        assert_eq!(
            std::fs::read_to_string(&update.backup_path).expect("read backup"),
            "split=zero\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read reset config"),
            RESET_CONFIG_TEMPLATE
        );
    }
}
