use crate::error::{Aria2Error, FatalError, Result};
use std::path::Path;

const DEFAULT_MARGIN_MB: u64 = 100;

pub fn available_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::statvfs;
        let stat = statvfs(path)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("statvfs失败: {}", e))))?;
        Ok(stat.blocks_available().saturating_mul(stat.block_size()))
    }

    #[cfg(target_family = "windows")]
    {
        std::fs::metadata(path)
            .map(|_| {
                let _ = path;
                u64::MAX / 2
            })
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("获取磁盘信息失败: {}", e))))
    }
}

pub fn has_enough_space(path: &Path, required: u64) -> bool {
    available_space(path).map_or(false, |avail| avail >= required)
}

pub fn check_with_margin(path: &Path, required: u64, margin_mb: Option<u64>) -> Result<()> {
    let margin = margin_mb.unwrap_or(DEFAULT_MARGIN_MB) * 1024 * 1024;
    let total_needed = required.saturating_add(margin);
    let avail = available_space(path)?;
    if avail < total_needed {
        Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted))
    } else {
        Ok(())
    }
}

pub fn total_space(path: &Path) -> Result<u64> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };

    #[cfg(target_family = "unix")]
    {
        use std::os::unix::fs::statvfs;
        let stat = statvfs(path)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("statvfs失败: {}", e))))?;
        Ok(stat.blocks().saturating_mul(stat.block_size()))
    }

    #[cfg(target_family = "windows")]
    {
        std::fs::metadata(path)
            .map(|_| u64::MAX)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("获取磁盘信息失败: {}", e))))
    }
}
